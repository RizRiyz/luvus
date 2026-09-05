//! Extensions JSON API handlers.

use super::*;
use super::{params::*, projection::*};

impl App {
    pub(super) fn api_theme_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        Ok(self.theme_registry.list_json(&self.config.theme))
    }

    pub(super) fn api_theme_path(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        Ok(json!({
            "type": "theme_path",
            "path": crate::theme::themes_dir().display().to_string(),
        }))
    }

    pub(super) fn api_theme_use(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = p.get("id").and_then(Value::as_str).ok_or_else(|| {
                (
                    "invalid_request".to_string(),
                    "theme.use needs an id".to_string(),
                )
            })?;
            if self.theme_registry.get(id).is_none() {
                return Err((
                    "not_found".to_string(),
                    format!("theme `{id}` is not installed"),
                ));
            }
            self.apply_theme(id);
            Ok(json!({"type": "theme_selected", "id": self.config.theme}))
        }
    }

    // ── ui / appearance ──
    pub(super) fn api_ui_sidebar(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            // `side` selects left (default) or right (docs/29).
            let side = match p.get("side").and_then(|v| v.as_str()) {
                Some("right") => crate::app::Side::Right,
                _ => crate::app::Side::Left,
            };
            if let Some(w) = param_usize(p, "width") {
                self.set_side_width(side, w as u16);
            }
            if let Some(v) = p.get("visible").and_then(|v| v.as_bool()) {
                self.sidebars.get_mut(side).visible = v;
            }
            let s = self.sidebars.get(side);
            Ok(json!({
                "type": "ok",
                "width": s.width,
                "visible": s.visible,
            }))
        }
    }

    // A module pushes rows into its sidebar dock (docs/29, DOCK-4).
    // A one-line confirmation, the same transient toast a copy shows.
    pub(super) fn api_ui_toast(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let text = req_str(p, "text")?;
            self.show_toast(text.chars().take(120).collect::<String>());
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_ui_agent_title_push(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = method;
        let titles = p.get("titles").and_then(Value::as_array).ok_or_else(|| {
            (
                "invalid_request".to_string(),
                "titles must be a JSON array".to_string(),
            )
        })?;
        if titles.len() > MAX_AGENT_ROW_TITLES {
            return Err(("invalid_request".to_string(), "too many titles".to_string()));
        }
        let mut changed = false;
        for item in titles {
            changed |= self.apply_agent_row_title_item(item)?;
        }
        Ok(json!({"type":"ok", "changed": changed}))
    }

    pub(super) fn api_ui_agent_title_clear(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = method;
        let pane = match p.get("pane") {
            Some(value) => Some(self.parse_agent_row_title_pane(value)?),
            None => None,
        };
        let session = agent_row_title_session(p.get("agent"), p.get("session_id"))?;
        let changed = self.clear_agent_row_titles(pane, session);
        Ok(json!({"type":"ok", "changed": changed}))
    }

    fn parse_agent_row_title_pane(&self, value: &Value) -> Result<PaneId, (String, String)> {
        let id = value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
            .and_then(|id| u32::try_from(id).ok())
            .map(PaneId)
            .ok_or_else(|| ("invalid_request".into(), "invalid pane".into()))?;
        if !self.panes.contains_key(&id) {
            return Err(("not_found".into(), "pane not found".into()));
        }
        Ok(id)
    }

    fn apply_agent_row_title_item(&mut self, item: &Value) -> Result<bool, (String, String)> {
        if !item.is_object() {
            return Err((
                "invalid_request".into(),
                "each title must be an object".into(),
            ));
        }
        let title =
            sanitize_agent_row_title(item.get("title").and_then(Value::as_str).unwrap_or(""))?;
        let pane = match item.get("pane") {
            Some(value) => Some(self.parse_agent_row_title_pane(value)?),
            None => None,
        };
        let session = agent_row_title_session(item.get("agent"), item.get("session_id"))?;
        if pane.is_none() && session.is_none() {
            return Err((
                "invalid_request".into(),
                "each title needs pane or agent+session_id".into(),
            ));
        }
        let mut changed = false;
        if let Some(pane) = pane {
            changed |= self.set_agent_row_title_for_pane(pane, title.clone());
        }
        if let Some((agent, session_id)) = session {
            changed |= self.set_agent_row_title_for_session(agent, session_id, title);
        }
        Ok(changed)
    }

    pub(super) fn api_ui_dock_push(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Ok(json!({"type":"error","message":"dock id required"}));
            }
            let title = p
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let placement = match p.get("placement").and_then(|v| v.as_str()) {
                Some("right") | Some("sidebar.right") => crate::app::Side::Right,
                _ => crate::app::Side::Left,
            };
            let rows = p
                .get("rows")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|r| {
                            // A span with no text draws nothing, so drop it
                            // here: a malformed list (`["a", "b"]`, `[{}]`)
                            // then parses to no spans and the row falls
                            // back to `text` instead of rendering blank.
                            let spans: Vec<crate::app::DockSpan> = r
                                .get("spans")
                                .and_then(|v| v.as_array())
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|sp| {
                                            let text = sp.get("text")?.as_str()?;
                                            (!text.is_empty()).then(|| crate::app::DockSpan {
                                                text: text.to_string(),
                                                tone: sp
                                                    .get("tone")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string()),
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            // `text` is what a click hands to the action. A
                            // spans-only row gets the joined span text, so
                            // moving a row from `text` to `spans` cannot
                            // leave its action with an empty target.
                            let text = r
                                .get("text")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| {
                                    spans.iter().map(|sp| sp.text.as_str()).collect()
                                });
                            crate::app::DockRow {
                                text,
                                dot: r.get("dot").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                tone: r
                                    .get("tone")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                                spans,
                                action: r
                                    .get("action")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                                value: r
                                    .get("value")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                                // Right-click menu for this row (docs/52).
                                // Absent — every module written before this —
                                // leaves the row with no menu, as before. An
                                // entry with no `action` is a divider.
                                menu: r
                                    .get("menu")
                                    .and_then(|v| v.as_array())
                                    .map(|items| {
                                        items
                                            .iter()
                                            .map(|it| crate::app::DockRowMenuItem {
                                                title: it
                                                    .get("title")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                action: it
                                                    .get("action")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                value: it
                                                    .get("value")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string()),
                                                destructive: it
                                                    .get("destructive")
                                                    .and_then(|v| v.as_bool())
                                                    .unwrap_or(false),
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.push_module_dock(id, title, placement, rows);
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_ui_dock_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let arr: Vec<Value> = self
                .docks_flat()
                .iter()
                .map(|k| {
                    let side = match self.sidebars.side_of(k) {
                        Some(crate::app::Side::Right) => "right",
                        _ => "left",
                    };
                    json!({"id": k.id(), "side": side})
                })
                .collect();
            Ok(json!({"type":"dock_list","docks":arr}))
        }
    }

    pub(super) fn api_ui_dock_move(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Ok(json!({"type":"error","message":"dock id required"}));
            }
            let side = match p.get("side").and_then(|v| v.as_str()) {
                Some("right") => crate::app::Side::Right,
                _ => crate::app::Side::Left,
            };
            if self.move_dock(&crate::app::DockKind::from_id(id), side) {
                Ok(json!({"type":"ok"}))
            } else {
                Ok(json!({"type":"error","message":"sidebar is full (max 3 docks)"}))
            }
        }
    }

    pub(super) fn api_ui_bar_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let widgets: Vec<Value> = self
                .bar
                .declarations
                .iter()
                .map(|(key, declaration)| {
                    let live = self.bar.widgets.get(key);
                    let region = self
                        .config
                        .bars
                        .region_for(key, declaration.region)
                        .map(crate::bar::BarRegion::as_str);
                    json!({
                        "id": declaration.key.id,
                        "owner": declaration.key.owner,
                        "key": key,
                        "title": declaration.title,
                        "region": region,
                        "default_region": declaration.region.as_str(),
                        "priority": live.map_or(declaration.priority, |widget| widget.priority),
                        "live": live.is_some(),
                        "content": live.map(|widget| &widget.content),
                        "compact_content": live.map(|widget| &widget.compact_content),
                    })
                })
                .collect();
            Ok(json!({"type":"bar_list","widgets":widgets}))
        }
    }

    pub(super) fn api_ui_bar_push(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?;
            let owner = p.get("owner").and_then(Value::as_str);
            let declaration = self
                .bar
                .resolve_declaration(owner, id)
                .map_err(module_err)?
                .clone();
            if declaration.key.owner == "core" {
                return Err(module_err("core bar widgets cannot be updated".into()));
            }
            let content: Vec<crate::bar::BarSegment> = serde_json::from_value(
                p.get("content")
                    .cloned()
                    .ok_or_else(|| ("invalid_request".into(), "content is required".into()))?,
            )
            .map_err(|error| {
                (
                    "invalid_request".into(),
                    format!("invalid content: {error}"),
                )
            })?;
            let compact: Vec<crate::bar::BarSegment> = match p.get("compact_content") {
                Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
                    (
                        "invalid_request".into(),
                        format!("invalid compact_content: {error}"),
                    )
                })?,
                None => Vec::new(),
            };
            validate_bar_actions(self, &declaration.key.owner, &content)?;
            validate_bar_actions(self, &declaration.key.owner, &compact)?;
            let region = match p.get("region").and_then(Value::as_str) {
                Some("top-right" | "top") => crate::bar::BarRegion::TopRight,
                Some("bottom-right" | "bottom") => crate::bar::BarRegion::BottomRight,
                Some(other) => {
                    return Err((
                        "invalid_request".into(),
                        format!("unknown bar region {other}"),
                    ))
                }
                None => declaration.region,
            };
            let priority = match p.get("priority") {
                None => declaration.priority,
                Some(value) => value
                    .as_u64()
                    .filter(|value| *value <= u8::MAX as u64)
                    .map(|value| value as u8)
                    .ok_or_else(|| {
                        (
                            "invalid_request".into(),
                            "priority must be an integer from 0 to 255".into(),
                        )
                    })?,
            };
            let widget = crate::bar::BarWidget::new(
                declaration.key.clone(),
                region,
                content,
                compact,
                priority,
            )
            .map_err(|error| ("invalid_request".into(), error))?;
            self.bar
                .allow_push(&declaration.key.owner, Instant::now())
                .map_err(|error| ("rate_limited".into(), error))?;
            let changed = self
                .bar
                .push_widget(widget)
                .map_err(|error| ("limit_exceeded".into(), error))?;
            Ok(json!({"type":"ok","changed":changed,"key":declaration.key.canonical()}))
        }
    }

    pub(super) fn api_ui_bar_move(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let declaration = self
                .bar
                .resolve_declaration(p.get("owner").and_then(Value::as_str), req_str(p, "id")?)
                .map_err(module_err)?
                .clone();
            let region = match req_str(p, "region")? {
                "top-right" | "top" => Some(crate::bar::BarRegion::TopRight),
                "bottom-right" | "bottom" => Some(crate::bar::BarRegion::BottomRight),
                "off" => None,
                other => {
                    return Err((
                        "invalid_request".into(),
                        format!("unknown bar region {other}"),
                    ))
                }
            };
            let key = declaration.key.canonical();
            if !self.config.bars.is_explicitly_placed(&key, region) {
                self.config.bars.place(&key, region);
                self.persist_config();
                self.bar.clear_geometry();
            }
            Ok(json!({"type":"ok","key":key,"region":region.map(crate::bar::BarRegion::as_str)}))
        }
    }

    pub(super) fn api_ui_bar_remove(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let declaration = self
                .bar
                .resolve_declaration(p.get("owner").and_then(Value::as_str), req_str(p, "id")?)
                .map_err(module_err)?
                .clone();
            if declaration.key.owner == "core" {
                return Err(module_err("core bar widgets cannot be removed".into()));
            }
            let removed = self.bar.remove_widget(&declaration.key.canonical());
            Ok(json!({"type":"ok","removed":removed}))
        }
    }

    pub(super) fn api_ui_notification_push(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let owner = p.get("owner").and_then(Value::as_str).map(String::from);
            let text = req_str(p, "text")?.to_string();
            let level: crate::bar::NotificationLevel =
                serde_json::from_value(p.get("level").cloned().unwrap_or_else(|| json!("info")))
                    .map_err(|error| {
                        ("invalid_request".into(), format!("invalid level: {error}"))
                    })?;
            let action = opt_str(p, "action");
            if let Some(owner) = owner.as_deref() {
                validate_bar_action(self, owner, action.as_deref())?;
            } else if action.is_some() {
                return Err((
                    "invalid_request".into(),
                    "an actionable notification requires its module owner".into(),
                ));
            }
            let ttl_ms = match p.get("ttl_ms") {
                None => 4_000,
                Some(value) => value.as_u64().filter(|ttl| *ttl > 0).ok_or_else(|| {
                    (
                        "invalid_request".into(),
                        "ttl_ms must be a positive integer".into(),
                    )
                })?,
            };
            let notification = crate::bar::NotificationPush {
                owner,
                text,
                level,
                ttl_ms,
                action,
                value: opt_str(p, "value"),
                dedupe_key: opt_str(p, "dedupe_key"),
            };
            notification
                .validate()
                .map_err(|error| ("invalid_request".into(), error))?;
            self.bar
                .allow_push(
                    notification
                        .owner
                        .as_deref()
                        .unwrap_or(crate::bar::UNOWNED_NOTIFICATION_OWNER),
                    Instant::now(),
                )
                .map_err(|error| ("rate_limited".into(), error))?;
            self.bar
                .push_notification(notification, Instant::now())
                .map_err(|error| ("invalid_request".into(), error))?;
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_ui_notification_clear(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let owner = p.get("owner").and_then(Value::as_str);
            let removed = self
                .bar
                .clear_notifications(owner, p.get("dedupe_key").and_then(Value::as_str));
            Ok(json!({"type":"ok","removed":removed}))
        }
    }

    // ── modules (docs/13) ──
    pub(super) fn api_module_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let arr: Vec<Value> = self.modules.modules.iter().map(module_json).collect();
            Ok(json!({"type":"module_list","modules":arr}))
        }
    }

    pub(super) fn api_module_info(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?;
            let m = self
                .modules
                .find(id)
                .ok_or_else(|| module_err(format!("no module {id}")))?;
            Ok(json!({
                "type": "module_info",
                "id": m.id,
                "name": m.manifest.name,
                "version": m.manifest.version,
                "description": m.manifest.description,
                "enabled": m.enabled,
                "runnable": m.is_runnable(),
                "source": m.source,
                "root": m.root.display().to_string(),
                "warning": m.warning,
                "platforms": m.manifest.platforms,
                "actions": m.manifest.actions.iter()
                    .map(|a| json!({"id": a.id, "title": a.title, "contexts": a.contexts})).collect::<Vec<_>>(),
                "panes": m.manifest.panes.iter()
                    .map(|pe| json!({"id": pe.id, "title": pe.title, "placement": pe.placement})).collect::<Vec<_>>(),
                "bars": m.manifest.bars.iter()
                    .map(|bar| json!({"id": bar.id, "title": bar.title, "region": bar.region.as_str(), "priority": bar.priority})).collect::<Vec<_>>(),
                "events": m.manifest.events.iter().map(|e| e.on.clone()).collect::<Vec<_>>(),
                "build_steps": m.manifest.build.len(),
            }))
        }
    }

    pub(super) fn api_module_link(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let path = req_str(p, "path")?;
            let enabled = !p.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);
            let source = p.get("source").and_then(|v| v.as_str()).map(String::from);
            let id = self
                .module_link_with(std::path::Path::new(path), enabled, source)
                .map_err(module_err)?;
            Ok(json!({"type":"module","id": id}))
        }
    }

    pub(super) fn api_module_unlink(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.module_unlink(req_str(p, "id")?).map_err(module_err)?;
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_module_uninstall(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.module_uninstall(req_str(p, "id")?)
                .map_err(module_err)?;
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_module_enable(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.module_set_enabled(req_str(p, "id")?, true)
                .map_err(module_err)?;
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_module_disable(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.module_set_enabled(req_str(p, "id")?, false)
                .map_err(module_err)?;
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_module_action_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let mut arr = Vec::new();
            for m in &self.modules.modules {
                for a in &m.manifest.actions {
                    arr.push(json!({
                        "module": m.id, "action": a.id,
                        "qualified": format!("{}.{}", m.id, a.id),
                        "title": a.title, "contexts": a.contexts,
                        "runnable": m.is_runnable(),
                    }));
                }
            }
            Ok(json!({"type":"module_action_list","actions":arr}))
        }
    }

    pub(super) fn api_module_action_invoke(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let action = p
                .get("id")
                .or_else(|| p.get("action"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "action id is required".to_string(),
                    )
                })?;
            let module = p.get("module").and_then(|v| v.as_str());
            let log_id = self
                .module_invoke_action(action, module, "api")
                .map_err(module_err)?;
            Ok(json!({"type":"module_command","log_id": log_id}))
        }
    }

    pub(super) fn api_module_log_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let filter = p
                .get("id")
                .or_else(|| p.get("module"))
                .and_then(|v| v.as_str());
            let limit = param_usize(p, "limit").unwrap_or(50);
            let logs: Vec<Value> = self
                .module_logs
                .iter()
                .rev()
                .filter(|l| filter.is_none_or(|f| l.module_id == f))
                .take(limit)
                .map(|l| serde_json::to_value(l).unwrap_or(Value::Null))
                .collect();
            Ok(json!({"type":"module_log_list","logs":logs}))
        }
    }

    pub(super) fn api_module_config_dir(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let dir = self
                .module_config_dir(req_str(p, "id")?)
                .map_err(module_err)?;
            Ok(json!({"type":"module_config_dir","dir": dir.display().to_string()}))
        }
    }

    pub(super) fn api_module_pane_open(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let module = p
                .get("module")
                .or_else(|| p.get("id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    (
                        "invalid_request".to_string(),
                        "module id is required".to_string(),
                    )
                })?;
            let entrypoint = req_str(p, "entrypoint")?;
            let placement = p.get("placement").and_then(|v| v.as_str());
            let id = self
                .module_open_pane(module, entrypoint, placement, "api")
                .map_err(module_err)?;
            Ok(json!({"type":"pane","pane": id.0.to_string()}))
        }
    }

    // ── module settings (docs/13 §3.6) ──
    pub(super) fn api_module_settings_list(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?.to_string();
            let values = self.module_settings(&id).map_err(module_err)?;
            let specs: Vec<Value> = self
                .modules
                .find(&id)
                .map(|m| {
                    m.manifest
                        .settings
                        .iter()
                        .map(|s| {
                            let v = values.get(&s.key).cloned().unwrap_or(Value::Null);
                            // A listing is the "show me everything" call and
                            // usually lands in a terminal, so a secret reports
                            // only whether it is set — same as the UI. Read the
                            // exact value with `module.settings.get {key}`.
                            let set = !matches!(&v, Value::Null)
                                && !v.as_str().is_some_and(|t| t.is_empty());
                            json!({
                                "key": s.key, "title": s.title, "type": s.kind,
                                "options": s.options, "min": s.min, "max": s.max,
                                "secret": s.secret, "set": set,
                                "value": if s.secret { Value::Null } else { v },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({"type":"module_settings","id": id,"settings": specs}))
        }
    }

    pub(super) fn api_module_settings_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?.to_string();
            let values = self.module_settings(&id).map_err(module_err)?;
            match p.get("key").and_then(|v| v.as_str()) {
                Some(k) => {
                    let v = values
                        .get(k)
                        .cloned()
                        .ok_or_else(|| module_err(format!("module {id} has no setting {k}")))?;
                    Ok(json!({"type":"module_setting","id": id,"key": k,"value": v}))
                }
                None => Ok(json!({"type":"module_settings","id": id,"values": values})),
            }
        }
    }

    pub(super) fn api_module_settings_set(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = req_str(p, "id")?.to_string();
            let key = req_str(p, "key")?.to_string();
            // Accept a JSON value or a bare string (what the CLI sends).
            let raw = p.get("value").cloned().unwrap_or(Value::Null);
            let v = self
                .module_set_setting(&id, &key, raw)
                .map_err(module_err)?;
            Ok(json!({"type":"module_setting","id": id,"key": key,"value": v}))
        }
    }

    pub(super) fn api_module_pane_focus(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
            self.focus_pane_global(id);
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_module_pane_close(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let id = self.resolve_pane(p)?.ok_or_else(not_found)?;
            self.close_pane(id);
            Ok(json!({"type":"ok"}))
        }
    }

    pub(super) fn api_mission_snapshot(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["scope", "workspace", "workspace_id"])?;
            let scope = match p.get("scope") {
                None => crate::mission::MissionScope::Workspace,
                Some(Value::String(scope)) if scope == "workspace" => {
                    crate::mission::MissionScope::Workspace
                }
                Some(Value::String(scope)) if scope == "all" => crate::mission::MissionScope::All,
                Some(_) => {
                    return Err((
                        "invalid_request".to_string(),
                        "scope must be workspace or all".to_string(),
                    ))
                }
            };
            let workspace = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
            if scope == crate::mission::MissionScope::Workspace
                && workspace >= self.workspaces.len()
            {
                return Err(workspace_update_error(
                    workspace,
                    WorkspaceUpdateError::NotFound,
                ));
            }
            if method == "mission.refresh" {
                self.request_mission_usage_refresh_for(scope, workspace);
                Ok(json!({
                    "type":"mission_refresh",
                    "scope":match scope { crate::mission::MissionScope::Workspace => "workspace", crate::mission::MissionScope::All => "all" },
                    "workspace":workspace.to_string(),
                    "refreshing":true,
                }))
            } else {
                Ok(self.mission_snapshot_value(scope, workspace))
            }
        }
    }

    pub(super) fn api_mission_open(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            let i = self.optional_socket_workspace(p)?.unwrap_or(self.active_ws);
            if i >= self.workspaces.len() {
                return Err(workspace_update_error(i, WorkspaceUpdateError::NotFound));
            }
            self.open_mission_control(i);
            Ok(json!({"type":"ok","mission": self.active_is_mission()}))
        }
    }
}
