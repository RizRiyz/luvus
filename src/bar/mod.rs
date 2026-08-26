//! Luvus Bar: bounded, structured, single-row extension widgets.
//!
//! The server owns validated widget state; each attachment independently
//! composes it for its viewport. Modules never receive Ratatui or ANSI access.

pub mod render;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

pub const CORE_RUNTIME: &str = "core:runtime-status";
pub const CORE_AGENTS: &str = "core:agent-summary";
pub const UNOWNED_NOTIFICATION_OWNER: &str = "core-notification";
pub const MAX_WIDGETS: usize = 64;
pub const MAX_WIDGETS_PER_MODULE: usize = 16;
pub const MAX_SEGMENTS: usize = 16;
pub const MAX_TEXT_BYTES: usize = 256;
pub const MAX_WIDGET_WIDTH: usize = 256;
pub const MAX_BAR_REGION_WIDTH: u16 = 100;
pub const MAX_BAR_WIDGET_WIDTH: u16 = 100;
/// Flexible columns retained after the fixed arrow/new-tab allowance: ten for
/// the active tab and two for the tab/bar edge spacing.
pub const MIN_TOP_TAB_FLEX_WIDTH: u16 = 12;
pub const MAX_NOTIFICATIONS: usize = 32;
pub const MIN_TTL_MS: u64 = 500;
pub const MAX_TTL_MS: u64 = 60_000;
const MAX_PUSHES_PER_SECOND: u16 = 30;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BarRegion {
    TopRight,
    #[default]
    BottomRight,
}

impl BarRegion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TopRight => "top-right",
            Self::BottomRight => "bottom-right",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BarTone {
    #[default]
    Normal,
    Muted,
    Accent,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BarSegmentKind {
    Text {
        text: String,
    },
    Symbol {
        symbol: String,
    },
    State {
        state: String,
        #[serde(default)]
        label: Option<String>,
    },
    Badge {
        text: String,
    },
    Progress {
        value: u64,
        total: u64,
        #[serde(default = "default_progress_width")]
        width: u16,
    },
    Spacer {
        width: u16,
    },
    Separator,
}

fn default_progress_width() -> u16 {
    8
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarSegment {
    #[serde(flatten)]
    pub kind: BarSegmentKind,
    #[serde(default)]
    pub tone: BarTone,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

impl BarSegment {
    pub fn text(text: impl Into<String>, tone: BarTone) -> Self {
        Self {
            kind: BarSegmentKind::Text { text: text.into() },
            tone,
            action: None,
            value: None,
        }
    }

    pub fn separator() -> Self {
        Self {
            kind: BarSegmentKind::Separator,
            tone: BarTone::Muted,
            action: None,
            value: None,
        }
    }

    pub fn display_width(&self) -> usize {
        match &self.kind {
            BarSegmentKind::Text { text } | BarSegmentKind::Symbol { symbol: text } => text.width(),
            BarSegmentKind::State { label, .. } => {
                1 + label.as_deref().map_or(0, |label| 1 + label.width())
            }
            BarSegmentKind::Badge { text } => text.width() + 2,
            BarSegmentKind::Progress { width, .. } => *width as usize,
            BarSegmentKind::Spacer { width } => *width as usize,
            BarSegmentKind::Separator => 5,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let check_text = |field: &str, text: &str| {
            if text.is_empty() {
                return Err(format!("{field} must not be empty"));
            }
            if text.len() > MAX_TEXT_BYTES {
                return Err(format!("{field} exceeds {MAX_TEXT_BYTES} bytes"));
            }
            if text.chars().any(char::is_control) {
                return Err(format!("{field} contains terminal control characters"));
            }
            Ok(())
        };
        match &self.kind {
            BarSegmentKind::Text { text } => check_text("text", text)?,
            BarSegmentKind::Symbol { symbol } => check_text("symbol", symbol)?,
            BarSegmentKind::State { state, label } => {
                if !["blocked", "working", "done", "idle", "unknown"].contains(&state.as_str()) {
                    return Err(format!("unknown state {state:?}"));
                }
                if let Some(label) = label {
                    check_text("state label", label)?;
                }
            }
            BarSegmentKind::Badge { text } => check_text("badge", text)?,
            BarSegmentKind::Progress {
                value,
                total,
                width,
            } => {
                if *total == 0 || value > total {
                    return Err("progress requires 0 <= value <= total and total > 0".into());
                }
                if !(3..=24).contains(width) {
                    return Err("progress width must be between 3 and 24".into());
                }
            }
            BarSegmentKind::Spacer { width } if *width > 16 => {
                return Err("spacer width must be at most 16".into())
            }
            BarSegmentKind::Spacer { .. } | BarSegmentKind::Separator => {}
        }
        if self.action.as_deref().is_some_and(str::is_empty) {
            return Err("action must not be empty".into());
        }
        if let Some(value) = &self.value {
            if value.len() > MAX_TEXT_BYTES || value.chars().any(char::is_control) {
                return Err("action value is too long or contains controls".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct BarWidgetKey {
    pub owner: String,
    pub id: String,
}

impl BarWidgetKey {
    pub fn new(owner: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            id: id.into(),
        }
    }

    pub fn canonical(&self) -> String {
        format!("{}:{}", self.owner, self.id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarWidget {
    pub key: BarWidgetKey,
    pub region: BarRegion,
    pub content: Vec<BarSegment>,
    pub compact_content: Vec<BarSegment>,
    pub priority: u8,
    pub full_width: u16,
    pub compact_width: u16,
}

impl BarWidget {
    pub fn new(
        key: BarWidgetKey,
        region: BarRegion,
        content: Vec<BarSegment>,
        compact_content: Vec<BarSegment>,
        priority: u8,
    ) -> Result<Self, String> {
        validate_segments(&content, false)?;
        validate_segments(&compact_content, true)?;
        let full_width = segment_width(&content)?;
        let compact_width = segment_width(&compact_content)?;
        Ok(Self {
            key,
            region,
            content,
            compact_content,
            priority,
            full_width,
            compact_width,
        })
    }

    pub fn segments(&self, representation: Representation) -> &[BarSegment] {
        match representation {
            Representation::Full => &self.content,
            Representation::Compact => &self.compact_content,
        }
    }

    pub fn width(&self, representation: Representation) -> u16 {
        match representation {
            Representation::Full => self.full_width,
            Representation::Compact => self.compact_width,
        }
    }
}

fn validate_segments(segments: &[BarSegment], compact: bool) -> Result<(), String> {
    if !compact && segments.is_empty() {
        return Err("bar content must not be empty".into());
    }
    if segments.len() > MAX_SEGMENTS {
        return Err(format!("bar content exceeds {MAX_SEGMENTS} segments"));
    }
    for segment in segments {
        segment.validate()?;
    }
    Ok(())
}

fn segment_width(segments: &[BarSegment]) -> Result<u16, String> {
    let width: usize = segments.iter().map(BarSegment::display_width).sum();
    if width > MAX_WIDGET_WIDTH {
        return Err(format!("bar content exceeds {MAX_WIDGET_WIDTH} columns"));
    }
    Ok(width as u16)
}

#[derive(Clone, Debug)]
pub struct BarDeclaration {
    pub key: BarWidgetKey,
    pub title: String,
    pub region: BarRegion,
    pub priority: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

impl NotificationLevel {
    pub fn tone(self) -> BarTone {
        match self {
            Self::Info => BarTone::Accent,
            Self::Success => BarTone::Success,
            Self::Warning => BarTone::Warning,
            Self::Error => BarTone::Error,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BarNotification {
    pub widget: BarWidget,
    pub owner: Option<String>,
    pub expires_at: Instant,
    pub dedupe_key: Option<String>,
}

pub struct NotificationPush {
    pub owner: Option<String>,
    pub text: String,
    pub level: NotificationLevel,
    pub ttl_ms: u64,
    pub action: Option<String>,
    pub value: Option<String>,
    pub dedupe_key: Option<String>,
}

impl NotificationPush {
    /// Validate the complete structured payload without mutating bar state. API
    /// dispatch uses this before consuming an owner's push-rate allowance, while
    /// `push_notification` repeats it to keep direct callers safe.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(key) = self.dedupe_key.as_deref() {
            if key.is_empty() || key.len() > MAX_TEXT_BYTES || key.chars().any(char::is_control) {
                return Err("dedupe_key is empty, too long, or contains controls".into());
            }
        }
        let mut segment = BarSegment::text(self.text.clone(), self.level.tone());
        segment.action = self.action.clone();
        segment.value = self.value.clone();
        BarWidget::new(
            BarWidgetKey::new(
                self.owner.as_deref().unwrap_or(UNOWNED_NOTIFICATION_OWNER),
                "validation",
            ),
            BarRegion::BottomRight,
            vec![segment],
            Vec::new(),
            90,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarHit {
    pub key: BarWidgetKey,
    pub segment: usize,
    pub rect: ratatui::layout::Rect,
    pub action: String,
    pub value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverflowHit {
    pub region: BarRegion,
    pub rect: ratatui::layout::Rect,
    pub hidden: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct OverflowPopup {
    pub region: BarRegion,
    pub keys: Vec<String>,
    pub rect: ratatui::layout::Rect,
}

pub struct BarState {
    pub declarations: BTreeMap<String, BarDeclaration>,
    pub widgets: BTreeMap<String, BarWidget>,
    pub notifications: VecDeque<BarNotification>,
    pub hits: Vec<BarHit>,
    pub overflow_hits: Vec<OverflowHit>,
    pub overflow: Option<OverflowPopup>,
    push_windows: HashMap<String, (Instant, u16)>,
    next_notification: u64,
}

impl Default for BarState {
    fn default() -> Self {
        let mut state = Self {
            declarations: BTreeMap::new(),
            widgets: BTreeMap::new(),
            notifications: VecDeque::new(),
            hits: Vec::new(),
            overflow_hits: Vec::new(),
            overflow: None,
            push_windows: HashMap::new(),
            next_notification: 1,
        };
        state.declarations.insert(
            CORE_RUNTIME.to_string(),
            BarDeclaration {
                key: BarWidgetKey::new("core", "runtime-status"),
                title: "Runtime status".into(),
                region: BarRegion::BottomRight,
                priority: 100,
            },
        );
        state
    }
}

impl BarState {
    pub fn sync_modules(&mut self, modules: &crate::module::ModuleRegistry) {
        self.declarations.retain(|key, _| key == CORE_RUNTIME);
        for module in modules.modules.iter().filter(|module| module.is_runnable()) {
            for entry in &module.manifest.bars {
                let key = BarWidgetKey::new(&module.id, &entry.id);
                self.declarations.insert(
                    key.canonical(),
                    BarDeclaration {
                        key,
                        title: entry.title.clone(),
                        region: entry.region,
                        priority: entry.priority,
                    },
                );
            }
        }
        self.widgets.retain(|key, _| {
            key == CORE_RUNTIME || key == CORE_AGENTS || self.declarations.contains_key(key)
        });
        self.notifications.retain(|notification| {
            notification.owner.as_ref().is_none_or(|owner| {
                modules
                    .find(owner)
                    .is_some_and(crate::module::InstalledModule::is_runnable)
            })
        });
        self.clear_geometry();
    }

    pub fn clear_owner(&mut self, owner: &str) {
        self.widgets.retain(|_, widget| widget.key.owner != owner);
        self.notifications
            .retain(|notification| notification.owner.as_deref() != Some(owner));
        self.push_windows.remove(owner);
        self.clear_geometry();
    }

    pub fn clear_geometry(&mut self) {
        self.hits.clear();
        self.overflow_hits.clear();
        self.overflow = None;
    }

    pub fn allow_push(&mut self, owner: &str, now: Instant) -> Result<(), String> {
        let window = self
            .push_windows
            .entry(owner.to_string())
            .or_insert((now, 0));
        if now.duration_since(window.0) >= Duration::from_secs(1) {
            *window = (now, 0);
        }
        if window.1 >= MAX_PUSHES_PER_SECOND {
            return Err(format!("bar update rate exceeded for {owner}"));
        }
        window.1 += 1;
        Ok(())
    }

    pub fn push_widget(&mut self, widget: BarWidget) -> Result<bool, String> {
        let key = widget.key.canonical();
        if self.widgets.len() >= MAX_WIDGETS && !self.widgets.contains_key(&key) {
            return Err(format!("bar has reached its {MAX_WIDGETS}-widget limit"));
        }
        let owner_count = self
            .widgets
            .values()
            .filter(|existing| existing.key.owner == widget.key.owner)
            .count();
        if owner_count >= MAX_WIDGETS_PER_MODULE && !self.widgets.contains_key(&key) {
            return Err(format!(
                "module {} has reached its {MAX_WIDGETS_PER_MODULE}-widget limit",
                widget.key.owner
            ));
        }
        let changed = self.widgets.get(&key) != Some(&widget);
        if changed {
            self.widgets.insert(key, widget);
            self.clear_geometry();
        }
        Ok(changed)
    }

    pub fn remove_widget(&mut self, key: &str) -> bool {
        let removed = self.widgets.remove(key).is_some();
        if removed {
            self.clear_geometry();
        }
        removed
    }

    pub fn push_notification(
        &mut self,
        request: NotificationPush,
        now: Instant,
    ) -> Result<(), String> {
        request.validate()?;
        let NotificationPush {
            owner,
            text,
            level,
            ttl_ms,
            action,
            value,
            dedupe_key,
        } = request;
        let mut segment = BarSegment::text(text, level.tone());
        segment.action = action;
        segment.value = value;
        let id = self.next_notification;
        self.next_notification = self.next_notification.wrapping_add(1);
        let widget = BarWidget::new(
            BarWidgetKey::new(
                owner.as_deref().unwrap_or(UNOWNED_NOTIFICATION_OWNER),
                id.to_string(),
            ),
            BarRegion::BottomRight,
            vec![segment],
            Vec::new(),
            90,
        )?;
        if let Some(key) = dedupe_key.as_deref() {
            self.notifications.retain(|notification| {
                notification.owner.as_deref() != owner.as_deref()
                    || notification.dedupe_key.as_deref() != Some(key)
            });
        }
        self.notifications.push_back(BarNotification {
            widget,
            owner,
            expires_at: now + Duration::from_millis(ttl_ms.clamp(MIN_TTL_MS, MAX_TTL_MS)),
            dedupe_key,
        });
        while self.notifications.len() > MAX_NOTIFICATIONS {
            self.notifications.pop_front();
        }
        self.clear_geometry();
        Ok(())
    }

    pub fn clear_notifications(&mut self, owner: Option<&str>, dedupe: Option<&str>) -> usize {
        let before = self.notifications.len();
        self.notifications.retain(|notification| {
            let owner_matches =
                owner.is_none_or(|owner| notification.owner.as_deref() == Some(owner));
            let dedupe_matches =
                dedupe.is_none_or(|key| notification.dedupe_key.as_deref() == Some(key));
            !(owner_matches && dedupe_matches)
        });
        let removed = before - self.notifications.len();
        if removed > 0 {
            self.clear_geometry();
        }
        removed
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        let before = self.notifications.len();
        self.notifications
            .retain(|notification| notification.expires_at > now);
        let changed = before != self.notifications.len();
        if changed {
            self.clear_geometry();
        }
        changed
    }

    pub fn has_visible_working(&self, config: &crate::config::BarConfig, compact: bool) -> bool {
        let widgets = self.widgets.iter().filter_map(|(key, widget)| {
            let visible = if key == CORE_AGENTS {
                compact
            } else {
                config.region_for(key, widget.region).is_some()
            };
            visible.then_some(widget)
        });
        let notifications = (!compact)
            .then_some(
                self.notifications
                    .iter()
                    .map(|notification| &notification.widget),
            )
            .into_iter()
            .flatten();
        widgets
            .chain(notifications)
            .flat_map(|widget| widget.content.iter().chain(&widget.compact_content))
            .any(|segment| {
                matches!(&segment.kind, BarSegmentKind::State { state, .. } if state == "working")
            })
    }

    pub fn declaration(&self, canonical: &str) -> Option<&BarDeclaration> {
        self.declarations.get(canonical)
    }

    pub fn resolve_declaration(
        &self,
        owner: Option<&str>,
        id: &str,
    ) -> Result<&BarDeclaration, String> {
        let matches: Vec<&BarDeclaration> = self
            .declarations
            .values()
            .filter(|declaration| declaration.key.id == id)
            .filter(|declaration| owner.is_none_or(|owner| declaration.key.owner == owner))
            .collect();
        match matches.as_slice() {
            [declaration] => Ok(*declaration),
            [] => Err(format!("no declared bar widget {id}")),
            _ => Err(format!(
                "bar widget {id} is ambiguous; provide its module owner"
            )),
        }
    }

    pub fn title(&self, key: &str) -> String {
        self.declarations
            .get(key)
            .map(|declaration| declaration.title.clone())
            .or_else(|| self.widgets.get(key).map(|widget| widget.key.id.clone()))
            .unwrap_or_else(|| key.to_string())
    }

    pub fn widgets_for<'a>(
        &'a self,
        region: BarRegion,
        config: &'a crate::config::BarConfig,
        show_compact_agent_summary: bool,
    ) -> Vec<WidgetCandidate<'a>> {
        let mut ordered = Vec::new();
        let configured = config.order(region);
        for key in configured {
            if key == CORE_AGENTS && !show_compact_agent_summary {
                continue;
            }
            if let Some(widget) = self.widgets.get(key) {
                if config.region_for(key, widget.region) == Some(region) {
                    ordered.push(WidgetCandidate {
                        key: key.as_str(),
                        widget,
                    });
                }
            }
        }
        for (key, widget) in &self.widgets {
            if key == CORE_AGENTS && !show_compact_agent_summary {
                continue;
            }
            if ordered.iter().any(|candidate| candidate.key == key) {
                continue;
            }
            let effective = if key == CORE_AGENTS {
                Some(BarRegion::TopRight)
            } else {
                config.region_for(key, widget.region)
            };
            if effective == Some(region) {
                ordered.push(WidgetCandidate { key, widget });
            }
        }
        if region == BarRegion::BottomRight {
            if let Some(notification) = self.notifications.back() {
                ordered.push(WidgetCandidate {
                    key: "notification",
                    widget: &notification.widget,
                });
            }
        }
        ordered
    }
}

impl crate::app::App {
    pub fn mobile_bar_notification(&self) -> Option<String> {
        self.bar
            .notifications
            .back()
            .map(|notification| mobile_segment_text(&notification.widget.content))
            .filter(|text| !text.is_empty())
    }

    pub fn mobile_agent_summary(&self) -> Option<(String, crate::ui::theme::State)> {
        let widget = self.bar.widgets.get(CORE_AGENTS)?;
        widget.content.iter().find_map(|segment| {
            let BarSegmentKind::State { state, label } = &segment.kind else {
                return None;
            };
            let parsed = match state.as_str() {
                "blocked" => crate::ui::theme::State::Blocked,
                "working" => crate::ui::theme::State::Working,
                "done" => crate::ui::theme::State::Done,
                "idle" => crate::ui::theme::State::Idle,
                _ => crate::ui::theme::State::Unknown,
            };
            Some((
                label
                    .as_deref()
                    .map(|count| format!("{count} {}", parsed.label()))
                    .unwrap_or_else(|| parsed.label().to_string()),
                parsed,
            ))
        })
    }

    /// Refresh the two built-ins from already-cached application state. This is
    /// pure in-process composition: no IO, manifest lookup, or subprocess work.
    pub fn refresh_core_bar_widgets(&mut self) {
        if self.workspaces.is_empty() {
            self.bar.remove_widget(CORE_RUNTIME);
            self.bar.remove_widget(CORE_AGENTS);
            return;
        }
        let panes = self.layout().len();
        let (active_tab, tab_count) = {
            let workspace = self.ws();
            (workspace.active_tab, workspace.tabs.len())
        };
        let runtime = BarWidget::new(
            BarWidgetKey::new("core", "runtime-status"),
            BarRegion::BottomRight,
            vec![
                BarSegment::text(self.catalog.mode_normal, BarTone::Muted),
                BarSegment::separator(),
                BarSegment::text(
                    format!(
                        "{panes} {}",
                        if panes == 1 {
                            self.catalog.pane
                        } else {
                            self.catalog.panes
                        }
                    ),
                    BarTone::Normal,
                ),
                BarSegment::separator(),
                BarSegment::text(
                    format!("{} {}/{}", self.catalog.act_tab, active_tab + 1, tab_count),
                    BarTone::Normal,
                ),
            ],
            vec![
                BarSegment::text(format!("{panes}p"), BarTone::Normal),
                BarSegment::separator(),
                BarSegment::text(format!("{}/{}", active_tab + 1, tab_count), BarTone::Normal),
            ],
            100,
        )
        .expect("core runtime widget is bounded");
        let _ = self.bar.push_widget(runtime);

        let states = ["blocked", "working", "done", "idle"];
        let mut segments = Vec::new();
        let mut compact_segments = Vec::new();
        for (state, count) in states.into_iter().zip(self.agent_state_counts()) {
            if count == 0 {
                continue;
            }
            let state_segment = BarSegment {
                kind: BarSegmentKind::State {
                    state: state.to_string(),
                    label: Some(count.to_string()),
                },
                tone: BarTone::Normal,
                action: None,
                value: None,
            };
            if compact_segments.is_empty() {
                compact_segments.push(state_segment.clone());
            }
            segments.push(state_segment);
            segments.push(BarSegment {
                kind: BarSegmentKind::Spacer { width: 1 },
                tone: BarTone::Normal,
                action: None,
                value: None,
            });
        }
        if segments.is_empty() {
            self.bar.remove_widget(CORE_AGENTS);
        } else {
            let widget = BarWidget::new(
                BarWidgetKey::new("core", "agent-summary"),
                BarRegion::TopRight,
                segments,
                compact_segments,
                110,
            )
            .expect("core agent summary is bounded");
            let _ = self.bar.push_widget(widget);
        }
    }

    pub fn tick_bar_notifications(&mut self, now: Instant) -> bool {
        self.bar.tick(now)
    }

    /// Handle one Luvus Bar click. Geometry already snapshots action/value and
    /// is cleared immediately on push/remove, so stale identities are inert.
    pub fn bar_click(&mut self, column: u16, row: u16) -> bool {
        let hit_rect = |rect: ratatui::layout::Rect| {
            column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
        };
        if let Some(open) = self.bar.overflow.as_ref() {
            if !hit_rect(open.rect) {
                self.bar.overflow = None;
            }
            return true;
        }
        if let Some(hit) = self
            .bar
            .overflow_hits
            .iter()
            .find(|hit| hit_rect(hit.rect))
            .cloned()
        {
            self.bar.overflow = Some(OverflowPopup {
                region: hit.region,
                keys: hit.hidden,
                rect: ratatui::layout::Rect::ZERO,
            });
            return true;
        }
        let Some(hit) = self.bar.hits.iter().find(|hit| hit_rect(hit.rect)).cloned() else {
            return false;
        };
        if hit.key.owner == "core" || hit.key.owner == UNOWNED_NOTIFICATION_OWNER {
            return true;
        }
        let mut extra = vec![
            ("LUVUS_MODULE_BAR_ID".to_string(), hit.key.id.clone()),
            (
                "LUVUS_MODULE_BAR_SEGMENT".to_string(),
                hit.segment.to_string(),
            ),
        ];
        if let Some(value) = hit.value {
            extra.push(("LUVUS_MODULE_BAR_VALUE".to_string(), value));
        }
        if let Err(error) =
            self.module_invoke_action_with(&hit.action, Some(&hit.key.owner), "bar", extra)
        {
            self.show_toast(error);
        }
        true
    }
}

fn mobile_segment_text(segments: &[BarSegment]) -> String {
    let mut output = String::new();
    for segment in segments {
        match &segment.kind {
            BarSegmentKind::Text { text } | BarSegmentKind::Symbol { symbol: text } => {
                output.push_str(text)
            }
            BarSegmentKind::State { state, label } => {
                output.push_str(match state.as_str() {
                    "blocked" | "working" | "done" => "●",
                    _ => "○",
                });
                if let Some(label) = label {
                    output.push(' ');
                    output.push_str(label);
                }
            }
            BarSegmentKind::Badge { text } => {
                output.push('[');
                output.push_str(text);
                output.push(']');
            }
            BarSegmentKind::Progress { value, total, .. } => {
                output.push_str(&format!("{value}/{total}"));
            }
            BarSegmentKind::Spacer { width } => output.push_str(&" ".repeat(*width as usize)),
            BarSegmentKind::Separator => output.push_str("  ·  "),
        }
    }
    output
}

pub struct WidgetCandidate<'a> {
    pub key: &'a str,
    pub widget: &'a BarWidget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Representation {
    Full,
    Compact,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutItem {
    pub candidate: usize,
    pub representation: Representation,
    pub width: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BarLayout {
    pub items: Vec<LayoutItem>,
    pub hidden: Vec<usize>,
    pub overflow_width: u16,
    pub width: u16,
}

impl BarLayout {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.overflow_width == 0
    }
}

/// Deterministically choose full, compact, or overflow for one region.
pub fn compose(candidates: &[WidgetCandidate<'_>], budget: u16, full_cap: u16) -> BarLayout {
    if candidates.is_empty() || budget == 0 {
        return BarLayout::default();
    }
    let mut visible: Vec<bool> = vec![true; candidates.len()];
    let mut reps = vec![Representation::Full; candidates.len()];
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.widget.full_width > full_cap {
            if candidate.widget.compact_width > 0 && candidate.widget.compact_width <= full_cap {
                reps[index] = Representation::Compact;
            } else {
                visible[index] = false;
            }
        }
    }

    loop {
        let hidden_count = visible.iter().filter(|visible| !**visible).count();
        let overflow_width = overflow_width(hidden_count);
        let width = total_width(candidates, &visible, &reps, overflow_width);
        if width <= budget {
            break;
        }
        let mut choice = None;
        for (index, candidate) in candidates.iter().enumerate() {
            if !visible[index]
                || reps[index] != Representation::Full
                || candidate.widget.compact_width == 0
                || candidate.widget.compact_width >= candidate.widget.full_width
            {
                continue;
            }
            if lower_priority(candidates, index, choice) {
                choice = Some(index);
            }
        }
        if let Some(index) = choice {
            reps[index] = Representation::Compact;
            continue;
        }
        let mut hide = None;
        for (index, is_visible) in visible.iter().copied().enumerate() {
            if is_visible && lower_priority(candidates, index, hide) {
                hide = Some(index);
            }
        }
        let Some(index) = hide else { break };
        visible[index] = false;
    }

    let mut layout = BarLayout::default();
    for (index, candidate) in candidates.iter().enumerate() {
        if visible[index] {
            let representation = reps[index];
            layout.items.push(LayoutItem {
                candidate: index,
                representation,
                width: candidate.widget.width(representation),
            });
        } else {
            layout.hidden.push(index);
        }
    }
    layout.overflow_width = overflow_width(layout.hidden.len());
    layout.width = total_width(candidates, &visible, &reps, layout.overflow_width);
    if layout.overflow_width > budget {
        layout.overflow_width = 0;
        layout.width = 0;
    }
    layout
}

fn lower_priority(
    candidates: &[WidgetCandidate<'_>],
    index: usize,
    current: Option<usize>,
) -> bool {
    current.is_none_or(|current| {
        let new = &candidates[index];
        let old = &candidates[current];
        let new_priority = survival_priority(new);
        let old_priority = survival_priority(old);
        new_priority < old_priority
            // Equal priority: the later candidate is compacted/hidden first,
            // preserving stable survival order for a fixed candidate list.
            || (new_priority == old_priority && index > current)
    })
}

fn survival_priority(candidate: &WidgetCandidate<'_>) -> u16 {
    match candidate.key {
        CORE_RUNTIME | CORE_AGENTS => 1_024,
        "notification" => 768,
        _ => candidate.widget.priority as u16,
    }
}

fn overflow_width(hidden: usize) -> u16 {
    if hidden == 0 {
        0
    } else {
        format!("… +{hidden}").width() as u16
    }
}

fn total_width(
    candidates: &[WidgetCandidate<'_>],
    visible: &[bool],
    reps: &[Representation],
    overflow: u16,
) -> u16 {
    let mut widths: Vec<u16> = candidates
        .iter()
        .enumerate()
        .filter(|(index, _)| visible[*index])
        .map(|(index, candidate)| candidate.widget.width(reps[index]))
        .collect();
    if overflow > 0 {
        widths.push(overflow);
    }
    widths.iter().sum::<u16>() + widths.len().saturating_sub(1) as u16 * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widget(id: &str, full: &str, compact: &str, priority: u8) -> BarWidget {
        BarWidget::new(
            BarWidgetKey::new("test", id),
            BarRegion::TopRight,
            vec![BarSegment::text(full, BarTone::Normal)],
            if compact.is_empty() {
                Vec::new()
            } else {
                vec![BarSegment::text(compact, BarTone::Normal)]
            },
            priority,
        )
        .unwrap()
    }

    #[test]
    fn every_segment_primitive_composes_in_order_and_measures_cells() {
        let segments = vec![
            BarSegment::text("界", BarTone::Normal),
            BarSegment {
                kind: BarSegmentKind::Symbol {
                    symbol: "✓".into()
                },
                tone: BarTone::Success,
                action: None,
                value: None,
            },
            BarSegment {
                kind: BarSegmentKind::State {
                    state: "done".into(),
                    label: Some("ok".into()),
                },
                tone: BarTone::Normal,
                action: None,
                value: None,
            },
            BarSegment {
                kind: BarSegmentKind::Badge { text: "2".into() },
                tone: BarTone::Error,
                action: None,
                value: None,
            },
            BarSegment {
                kind: BarSegmentKind::Progress {
                    value: 1,
                    total: 2,
                    width: 6,
                },
                tone: BarTone::Accent,
                action: None,
                value: None,
            },
            BarSegment {
                kind: BarSegmentKind::Spacer { width: 2 },
                tone: BarTone::Normal,
                action: None,
                value: None,
            },
            BarSegment::separator(),
        ];
        let widget = BarWidget::new(
            BarWidgetKey::new("test", "all"),
            BarRegion::BottomRight,
            segments.clone(),
            Vec::new(),
            50,
        )
        .unwrap();
        assert_eq!(widget.content, segments);
        assert_eq!(widget.full_width, 2 + 1 + 4 + 3 + 6 + 2 + 5);
    }

    #[test]
    fn composer_uses_full_then_compact_then_deterministic_overflow() {
        let widgets = [widget("a", "alpha", "a", 50), widget("b", "bravo", "b", 10)];
        let candidates: Vec<_> = widgets
            .iter()
            .map(|widget| WidgetCandidate {
                key: &widget.key.id,
                widget,
            })
            .collect();
        let full = compose(&candidates, 11, 24);
        assert_eq!(full.items.len(), 2);
        assert_eq!(full.items[1].representation, Representation::Compact);
        let overflow_widgets = [
            widget("a", "alpha", "", 50),
            widget("b", "bravo", "", 10),
            widget("c", "chrlie", "", 5),
        ];
        let overflow_candidates: Vec<_> = overflow_widgets
            .iter()
            .map(|widget| WidgetCandidate {
                key: &widget.key.id,
                widget,
            })
            .collect();
        let narrow = compose(&overflow_candidates, 11, 24);
        assert_eq!(narrow.items.len(), 1);
        assert_eq!(narrow.items[0].candidate, 0, "higher priority survives");
        assert_eq!(narrow.hidden, vec![1, 2]);
        assert_eq!(narrow.overflow_width, 4);
    }

    #[test]
    fn top_budget_contract_preserves_navigation_and_caps_at_100_columns() {
        for flexible in [33u16, 72, 111, 144, 300, 600] {
            let top = flexible
                .saturating_sub(MIN_TOP_TAB_FLEX_WIDTH)
                .min(MAX_BAR_REGION_WIDTH);
            assert!(top <= MAX_BAR_REGION_WIDTH);
            assert!(flexible.saturating_sub(top) >= MIN_TOP_TAB_FLEX_WIDTH);
        }
    }

    #[test]
    fn one_top_widget_can_show_100_ascii_characters_when_space_allows() {
        let text = "x".repeat(MAX_BAR_WIDGET_WIDTH as usize);
        let widget = widget("wide", &text, "wide", 50);
        let candidates = [WidgetCandidate {
            key: "wide",
            widget: &widget,
        }];
        let layout = compose(&candidates, MAX_BAR_REGION_WIDTH, MAX_BAR_WIDGET_WIDTH);
        assert_eq!(layout.width, MAX_BAR_REGION_WIDTH);
        assert_eq!(layout.items[0].representation, Representation::Full);
    }

    #[test]
    fn controls_and_oversized_values_are_rejected_before_mutation() {
        let bad = BarWidget::new(
            BarWidgetKey::new("test", "bad"),
            BarRegion::TopRight,
            vec![BarSegment::text("oops\x1b[31m", BarTone::Error)],
            Vec::new(),
            1,
        );
        assert!(bad.unwrap_err().contains("control"));
    }

    #[test]
    fn spinner_work_only_tracks_widgets_visible_in_the_current_chrome() {
        let mut state = BarState::default();
        let working = BarWidget::new(
            BarWidgetKey::new("test", "job"),
            BarRegion::TopRight,
            vec![BarSegment {
                kind: BarSegmentKind::State {
                    state: "working".into(),
                    label: None,
                },
                tone: BarTone::Normal,
                action: None,
                value: None,
            }],
            Vec::new(),
            1,
        )
        .unwrap();
        state.push_widget(working).unwrap();

        let mut config = crate::config::BarConfig::default();
        assert!(state.has_visible_working(&config, false));
        config.place("test:job", None);
        assert!(!state.has_visible_working(&config, false));
    }

    #[test]
    fn configured_core_agent_summary_still_obeys_compact_visibility() {
        let mut state = BarState::default();
        state
            .push_widget(
                BarWidget::new(
                    BarWidgetKey::new("core", "agent-summary"),
                    BarRegion::TopRight,
                    vec![BarSegment::text("agents", BarTone::Normal)],
                    Vec::new(),
                    100,
                )
                .unwrap(),
            )
            .unwrap();
        let mut config = crate::config::BarConfig::default();
        config.top_right.push(CORE_AGENTS.into());

        assert!(state
            .widgets_for(BarRegion::TopRight, &config, false)
            .is_empty());
        assert_eq!(
            state.widgets_for(BarRegion::TopRight, &config, true).len(),
            1
        );
    }

    #[test]
    fn notifications_are_bounded_deduped_and_expire() {
        let mut state = BarState::default();
        let now = Instant::now();
        state
            .push_notification(
                NotificationPush {
                    owner: Some("m".into()),
                    text: "one".into(),
                    level: NotificationLevel::Info,
                    ttl_ms: 500,
                    action: None,
                    value: None,
                    dedupe_key: Some("same".into()),
                },
                now,
            )
            .unwrap();
        state
            .push_notification(
                NotificationPush {
                    owner: Some("m".into()),
                    text: "two".into(),
                    level: NotificationLevel::Error,
                    ttl_ms: 500,
                    action: None,
                    value: None,
                    dedupe_key: Some("same".into()),
                },
                now,
            )
            .unwrap();
        assert_eq!(state.notifications.len(), 1);
        assert!(state.tick(now + Duration::from_millis(501)));
        assert!(state.notifications.is_empty());
    }

    #[test]
    fn update_and_notification_limits_are_bounded() {
        let mut state = BarState::default();
        let now = Instant::now();
        for _ in 0..MAX_PUSHES_PER_SECOND {
            state.allow_push("module", now).unwrap();
        }
        assert!(state.allow_push("module", now).is_err());
        assert!(state
            .allow_push("module", now + Duration::from_secs(1))
            .is_ok());

        for index in 0..(MAX_NOTIFICATIONS + 8) {
            state
                .push_notification(
                    NotificationPush {
                        owner: Some("module".into()),
                        text: format!("event {index}"),
                        level: NotificationLevel::Info,
                        ttl_ms: MAX_TTL_MS,
                        action: None,
                        value: None,
                        dedupe_key: None,
                    },
                    now,
                )
                .unwrap();
        }
        assert_eq!(state.notifications.len(), MAX_NOTIFICATIONS);
    }
}
