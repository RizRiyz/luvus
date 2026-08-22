//! `luvus-module.toml` — the module manifest: identity + declared argv commands
//! (actions, event hooks, panes, docks, bars, settings, startup + build steps). Parsed
//! with serde; validated to mirror the spec in docs/13 §3.1.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MANIFEST_FILE: &str = "luvus-module.toml";
pub const LEGACY_MANIFEST_FILE: &str = "bohay-module.toml";
const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Serialize, Deserialize)]
pub struct ModuleManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(alias = "min_bohay_version")]
    pub min_luvus_version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub build: Vec<Build>,
    /// One-shot commands run for each enabled module once the session is
    /// restored and the API socket is up (docs/13 §3.7). This is how a module
    /// repopulates its docks after a restart.
    #[serde(default)]
    pub startup: Vec<StartupHook>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub events: Vec<EventHook>,
    #[serde(default)]
    pub panes: Vec<PaneEntry>,
    /// Sidebar docks this module contributes (docs/29). The module pushes their
    /// content over the socket (`ui.dock.push`); luvus owns rendering.
    #[serde(default)]
    pub docks: Vec<DockEntry>,
    /// Compact single-row Luvus Bar widgets. A declaration grants ownership;
    /// content arrives later through `luvus bar push` (`ui.bar.push` on the API).
    #[serde(default)]
    pub bars: Vec<BarWidgetEntry>,
    /// User-editable settings rendered in Settings → Modules (docs/13 §3.6).
    #[serde(default)]
    pub settings: Vec<SettingSpec>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Build {
    pub command: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StartupHook {
    pub command: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub title: String,
    /// Right-click menus this action is offered in: `pane`, `workspace`
    /// (alias `node`), `agent`, `tab`. Omit for an action that is only ever
    /// invoked from the CLI, socket, or another module.
    #[serde(default)]
    pub contexts: Option<Vec<String>>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    pub command: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EventHook {
    pub on: String,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    pub command: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PaneEntry {
    pub id: String,
    pub title: String,
    #[serde(default = "default_placement")]
    pub placement: String,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    pub command: Vec<String>,
}

fn default_placement() -> String {
    "split".to_string()
}

/// A sidebar dock a module contributes (docs/29). `placement` is the default
/// side (`sidebar.left` / `sidebar.right`); the user can move it afterwards.
#[derive(Clone, Serialize, Deserialize)]
pub struct DockEntry {
    pub id: String,
    pub title: String,
    #[serde(default = "default_dock_placement")]
    pub placement: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BarWidgetEntry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub region: crate::bar::BarRegion,
    #[serde(default = "default_bar_priority")]
    pub priority: u8,
}

fn default_bar_priority() -> u8 {
    50
}

fn default_dock_placement() -> String {
    "sidebar.left".to_string()
}

/// The type of a declared setting, which picks its Settings-tab control.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SettingKind {
    /// An on/off toggle, like the built-in notification switches.
    Bool,
    /// A free-text value, edited in a small inline prompt.
    #[default]
    String,
    /// A whole number stepped with `‹ ›`, bounded by `min`/`max`.
    Number,
    /// One of `options`, cycled with `‹ ›`.
    Enum,
}

/// One user-editable setting a module declares. Values live in the module's
/// config dir (`settings.json`) and reach every command as
/// `LUVUS_MODULE_SETTINGS_JSON` plus `LUVUS_SETTING_<KEY>`.
#[derive(Clone, Serialize, Deserialize)]
pub struct SettingSpec {
    pub key: String,
    pub title: String,
    #[serde(rename = "type", default)]
    pub kind: SettingKind,
    #[serde(default)]
    pub default: Option<Value>,
    /// Choices for `type = "enum"`.
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
    #[serde(default)]
    pub step: Option<i64>,
    /// Hide the value in the UI (shown as `••••`) — for tokens and keys.
    #[serde(default)]
    pub secret: bool,
}

impl SettingSpec {
    /// The value used when the user has not set one.
    pub fn default_value(&self) -> Value {
        if let Some(v) = &self.default {
            return v.clone();
        }
        match self.kind {
            SettingKind::Bool => Value::Bool(false),
            SettingKind::Number => Value::from(self.min.unwrap_or(0)),
            SettingKind::Enum => Value::String(self.options.first().cloned().unwrap_or_default()),
            SettingKind::String => Value::String(String::new()),
        }
    }
}

/// Right-click menus an action can be attached to. `node` is accepted as a
/// legacy alias of `workspace`.
pub const KNOWN_CONTEXTS: &[&str] = &["pane", "workspace", "node", "agent", "tab"];

impl ModuleManifest {
    pub fn path(root: &Path) -> std::path::PathBuf {
        let current = root.join(MANIFEST_FILE);
        if current.is_file() {
            let legacy = root.join(LEGACY_MANIFEST_FILE);
            if legacy.is_file() && std::fs::read(&current).ok() != std::fs::read(&legacy).ok() {
                eprintln!(
                    "warning: both {MANIFEST_FILE} and {LEGACY_MANIFEST_FILE} exist in {}; using {MANIFEST_FILE}",
                    root.display()
                );
            }
            current
        } else {
            root.join(LEGACY_MANIFEST_FILE)
        }
    }

    /// Read + validate the manifest at `<root>/luvus-module.toml`.
    pub fn load(root: &Path) -> Result<ModuleManifest, String> {
        let path = Self::path(root);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let m: ModuleManifest =
            toml::from_str(&text).map_err(|e| format!("invalid {}: {e}", path.display()))?;
        m.validate()?;
        Ok(m)
    }

    /// Validate identity, version gate, argv shape, and id uniqueness.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_module_id(&self.id) {
            return Err(format!(
                "invalid module id {:?} (use [a-z0-9:._-], ≤120)",
                self.id
            ));
        }
        if self.name.trim().is_empty() {
            return Err("name is required".to_string());
        }
        if self.version.trim().is_empty() {
            return Err("version is required".to_string());
        }
        if self.min_luvus_version.trim().is_empty() {
            return Err("min_luvus_version is required".to_string());
        }
        if version_gt(&self.min_luvus_version, HOST_VERSION) {
            return Err(format!(
                "module needs luvus ≥ {}, this is {HOST_VERSION}",
                self.min_luvus_version
            ));
        }
        if self.platforms.as_ref().is_some_and(|p| p.is_empty()) {
            return Err("platforms = [] is invalid (omit it for all platforms)".to_string());
        }
        for b in &self.build {
            check_argv(&b.command, "build")?;
            check_platforms(b.platforms.as_ref(), "build")?;
        }
        for (i, s) in self.startup.iter().enumerate() {
            check_argv(&s.command, &format!("startup {i}"))?;
            check_platforms(s.platforms.as_ref(), &format!("startup {i}"))?;
        }
        let mut action_ids = HashSet::new();
        for a in &self.actions {
            if !valid_local_id(&a.id) {
                return Err(format!(
                    "invalid action id {:?} (use [a-z0-9:_-], no dots)",
                    a.id
                ));
            }
            if !action_ids.insert(a.id.as_str()) {
                return Err(format!("duplicate action id: {}", a.id));
            }
            check_argv(&a.command, &format!("action {}", a.id))?;
            check_platforms(a.platforms.as_ref(), &format!("action {}", a.id))?;
            for c in a.contexts.iter().flatten() {
                if !KNOWN_CONTEXTS.contains(&c.as_str()) {
                    return Err(format!(
                        "action {}: unknown context {c:?} (use {})",
                        a.id,
                        KNOWN_CONTEXTS.join(" | ")
                    ));
                }
            }
        }
        let mut pane_ids = HashSet::new();
        for pe in &self.panes {
            if !valid_local_id(&pe.id) {
                return Err(format!(
                    "invalid pane id {:?} (use [a-z0-9:_-], no dots)",
                    pe.id
                ));
            }
            if !pane_ids.insert(pe.id.as_str()) {
                return Err(format!("duplicate pane id: {}", pe.id));
            }
            check_argv(&pe.command, &format!("pane {}", pe.id))?;
            check_platforms(pe.platforms.as_ref(), &format!("pane {}", pe.id))?;
        }
        for e in &self.events {
            check_argv(&e.command, &format!("event {}", e.on))?;
            check_platforms(e.platforms.as_ref(), &format!("event {}", e.on))?;
        }
        let mut setting_keys = HashSet::new();
        for s in &self.settings {
            if !valid_local_id(&s.key) {
                return Err(format!(
                    "invalid setting key {:?} (use [a-z0-9:_-], no dots)",
                    s.key
                ));
            }
            if !setting_keys.insert(s.key.as_str()) {
                return Err(format!("duplicate setting key: {}", s.key));
            }
            if s.title.trim().is_empty() {
                return Err(format!("setting {}: title is required", s.key));
            }
            if s.kind == SettingKind::Enum && s.options.is_empty() {
                return Err(format!("setting {}: enum needs a non-empty options", s.key));
            }
            if let (Some(lo), Some(hi)) = (s.min, s.max) {
                if lo > hi {
                    return Err(format!("setting {}: min is greater than max", s.key));
                }
            }
        }
        let mut dock_ids = HashSet::new();
        for d in &self.docks {
            if !valid_local_id(&d.id) {
                return Err(format!(
                    "invalid dock id {:?} (use [a-z0-9:_-], no dots)",
                    d.id
                ));
            }
            if !dock_ids.insert(d.id.as_str()) {
                return Err(format!("duplicate dock id: {}", d.id));
            }
        }
        let mut bar_ids = HashSet::new();
        for bar in &self.bars {
            if !valid_local_id(&bar.id) {
                return Err(format!(
                    "invalid bar id {:?} (use [a-z0-9:_-], no dots)",
                    bar.id
                ));
            }
            if !bar_ids.insert(bar.id.as_str()) {
                return Err(format!("duplicate bar id: {}", bar.id));
            }
            if bar.title.trim().is_empty()
                || bar.title.len() > crate::bar::MAX_TEXT_BYTES
                || bar.title.chars().any(char::is_control)
            {
                return Err(format!(
                    "bar {}: title is required, bounded, and must not contain controls",
                    bar.id
                ));
            }
        }
        Ok(())
    }

    /// Whether this module is allowed to run on the current OS.
    pub fn allowed_on_platform(&self) -> bool {
        allowed_on(self.platforms.as_ref())
    }

    /// Find an action by its local id, ignoring ones gated off this platform.
    pub fn action(&self, id: &str) -> Option<&Action> {
        self.actions
            .iter()
            .find(|a| a.id == id && allowed_on(a.platforms.as_ref()))
    }

    /// Actions offered in right-click menu `context` on this platform.
    pub fn actions_for_context(&self, context: &str) -> Vec<&Action> {
        self.actions
            .iter()
            .filter(|a| allowed_on(a.platforms.as_ref()))
            .filter(|a| {
                a.contexts
                    .iter()
                    .flatten()
                    .any(|c| c == context || (c == "node" && context == "workspace"))
            })
            .collect()
    }

    /// Find a setting spec by key.
    pub fn setting(&self, key: &str) -> Option<&SettingSpec> {
        self.settings.iter().find(|s| s.key == key)
    }

    /// Event `on` names that aren't known luvus events (non-fatal warnings).
    /// Used when wiring event hooks (MOD-3).
    #[allow(dead_code)]
    pub fn unknown_events(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| !KNOWN_EVENTS.contains(&e.on.as_str()))
            .map(|e| e.on.clone())
            .collect()
    }
}

/// Events a module may hook (docs/13 §3.5). Consumed by the hook runner (MOD-3)
/// and by `module doctor` to flag typos. Kept in step with the `emit_event`
/// call sites — `workspace.*` also answers to the legacy `node.*` spelling.
pub const KNOWN_EVENTS: &[&str] = &[
    "workspace.created",
    "workspace.closed",
    "node.created",
    "node.closed",
    "tab.created",
    "tab.closed",
    "tab.moved",
    "pane.created",
    "pane.closed",
    "pane.forked",
    "pane.moved",
    "pane.agent_status_changed",
    "agent.hook",
    "task.added",
    "task.claimed",
    "task.started",
    "task.updated",
    "task.ready",
    "task.done",
    "task.released",
    "task.deleted",
    "task.merged",
    "task.merge_conflict",
    "task.needs_compaction",
    "task.gate_running",
    "task.gate_passed",
    "task.gate_failed",
    "lease.acquired",
    "lease.released",
];

/// Whether an item-level `platforms` list (or its absence) allows this OS.
pub fn allowed_on(platforms: Option<&Vec<String>>) -> bool {
    match platforms {
        None => true,
        Some(list) => list.iter().any(|p| p == current_platform()),
    }
}

fn check_platforms(platforms: Option<&Vec<String>>, what: &str) -> Result<(), String> {
    if platforms.is_some_and(|p| p.is_empty()) {
        return Err(format!(
            "{what}: platforms = [] is invalid (omit it for all platforms)"
        ));
    }
    Ok(())
}

fn check_argv(argv: &[String], what: &str) -> Result<(), String> {
    if argv.is_empty() {
        return Err(format!("{what}: command must be a non-empty argv array"));
    }
    if argv.iter().any(|a| a.is_empty()) {
        return Err(format!("{what}: command has an empty argument"));
    }
    Ok(())
}

/// Module id: `[a-z0-9:._-]`, 1..=120. Dots allowed (e.g. `you.git-status`).
fn valid_module_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 120
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b':' | b'.' | b'_' | b'-')
        })
}

/// Local (action/pane) id: like a module id but no dots — a qualified id is
/// `{module}.{local}`.
fn valid_local_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 120
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b':' | b'_' | b'-')
        })
}

pub fn current_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "other"
    }
}

/// `a > b` comparing dotted numeric versions (missing components = 0).
fn version_gt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    for i in 0..va.len().max(vb.len()) {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_a_legacy_manifest_and_version_field() {
        let root =
            std::env::temp_dir().join(format!("luvus-legacy-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(LEGACY_MANIFEST_FILE),
            r#"id = "you.legacy"
name = "Legacy"
version = "1.0.0"
min_bohay_version = "0.8.3"
"#,
        )
        .unwrap();
        let manifest = ModuleManifest::load(&root).unwrap();
        assert_eq!(manifest.min_luvus_version, "0.8.3");
        assert_eq!(ModuleManifest::path(&root), root.join(LEGACY_MANIFEST_FILE));
        let _ = std::fs::remove_dir_all(root);
    }

    fn base() -> ModuleManifest {
        ModuleManifest {
            id: "you.git-status".into(),
            name: "Git Status".into(),
            version: "0.1.0".into(),
            min_luvus_version: "0.1.0".into(),
            description: None,
            platforms: None,
            build: vec![],
            startup: vec![],
            actions: vec![],
            events: vec![],
            panes: vec![],
            docks: vec![],
            bars: vec![],
            settings: vec![],
        }
    }

    fn action(id: &str) -> Action {
        Action {
            id: id.into(),
            title: id.into(),
            contexts: None,
            platforms: None,
            command: vec!["echo".into(), "hi".into()],
        }
    }

    #[test]
    fn valid_manifest_passes() {
        let mut m = base();
        m.actions.push(action("refresh"));
        assert!(m.validate().is_ok());
    }

    #[test]
    fn bar_declarations_validate_ids_titles_regions_and_duplicates() {
        let parsed: ModuleManifest = toml::from_str(
            r#"
id = "you.ci"
name = "CI"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[bars]]
id = "status"
title = "CI status"
region = "top-right"
priority = 60
"#,
        )
        .unwrap();
        assert!(parsed.validate().is_ok());
        assert_eq!(parsed.bars[0].region, crate::bar::BarRegion::TopRight);

        let mut duplicate = parsed.clone();
        duplicate.bars.push(duplicate.bars[0].clone());
        assert!(duplicate
            .validate()
            .unwrap_err()
            .contains("duplicate bar id"));

        let mut unsafe_title = parsed.clone();
        unsafe_title.bars[0].title = "CI\u{1b}[31m".into();
        assert!(unsafe_title.validate().unwrap_err().contains("controls"));

        let unknown = toml::from_str::<ModuleManifest>(
            r#"
id = "you.ci"
name = "CI"
version = "0.1.0"
min_luvus_version = "0.1.0"
[[bars]]
id = "status"
title = "CI"
region = "middle"
"#,
        );
        assert!(unknown.is_err());
    }

    #[test]
    fn rejects_bad_ids_and_argv() {
        let mut m = base();
        m.id = "Bad Id".into();
        assert!(m.validate().is_err());

        let mut m = base();
        m.actions.push(action("has.dot")); // dots not allowed in local ids
        assert!(m.validate().is_err());

        let mut m = base();
        let mut a = action("ok");
        a.command = vec![]; // empty argv
        m.actions.push(a);
        assert!(m.validate().is_err());
    }

    #[test]
    fn action_contexts_gate_the_right_click_menus() {
        let mut m = base();
        let mut a = action("apply");
        a.contexts = Some(vec!["pane".into(), "node".into()]);
        m.actions.push(a);
        assert!(m.validate().is_ok());

        assert_eq!(m.actions_for_context("pane").len(), 1);
        // `node` is accepted as a legacy alias for `workspace`.
        assert_eq!(m.actions_for_context("workspace").len(), 1);
        assert_eq!(m.actions_for_context("agent").len(), 0);

        // A typo'd context is a manifest error, not a silently dead menu entry.
        let mut m = base();
        let mut a = action("apply");
        a.contexts = Some(vec!["sidebar".into()]);
        m.actions.push(a);
        assert!(m.validate().is_err());
    }

    #[test]
    fn item_platforms_gate_actions() {
        let mut m = base();
        let mut a = action("only-here");
        a.platforms = Some(vec![current_platform().to_string()]);
        a.contexts = Some(vec!["pane".into()]);
        m.actions.push(a);
        let mut b = action("never-here");
        b.platforms = Some(vec!["plan9".into()]);
        b.contexts = Some(vec!["pane".into()]);
        m.actions.push(b);
        assert!(m.validate().is_ok());

        assert!(m.action("only-here").is_some());
        assert!(m.action("never-here").is_none(), "gated off this platform");
        assert_eq!(m.actions_for_context("pane").len(), 1);
    }

    #[test]
    fn settings_validate_and_default() {
        let mut m = base();
        m.settings.push(SettingSpec {
            key: "mode".into(),
            title: "Mode".into(),
            kind: SettingKind::Enum,
            default: None,
            options: vec!["fast".into(), "slow".into()],
            min: None,
            max: None,
            step: None,
            secret: false,
        });
        assert!(m.validate().is_ok());
        // An enum with no explicit default falls back to its first option.
        assert_eq!(m.setting("mode").unwrap().default_value(), "fast");

        // enum without options is refused
        let mut bad = base();
        bad.settings.push(SettingSpec {
            key: "mode".into(),
            title: "Mode".into(),
            kind: SettingKind::Enum,
            default: None,
            options: vec![],
            min: None,
            max: None,
            step: None,
            secret: false,
        });
        assert!(bad.validate().is_err());

        // duplicate keys are refused
        let mut dup = base();
        for _ in 0..2 {
            dup.settings.push(SettingSpec {
                key: "token".into(),
                title: "Token".into(),
                kind: SettingKind::String,
                default: None,
                options: vec![],
                min: None,
                max: None,
                step: None,
                secret: true,
            });
        }
        assert!(dup.validate().is_err());
    }

    #[test]
    fn parses_a_full_v2_manifest() {
        let m: ModuleManifest = toml::from_str(
            r#"
id = "you.demo"
name = "Demo"
version = "0.1.0"
min_luvus_version = "0.1.0"

[[startup]]
command = ["./restore.sh"]

[[actions]]
id = "apply"
title = "Apply layout"
contexts = ["pane", "workspace"]
command = ["./apply.sh"]

[[settings]]
key = "token"
title = "API token"
type = "string"
secret = true

[[settings]]
key = "limit"
title = "Row limit"
type = "number"
default = 20
min = 1
max = 99
step = 1
"#,
        )
        .expect("parses");
        m.validate().expect("validates");
        assert_eq!(m.startup.len(), 1);
        assert_eq!(m.actions_for_context("workspace").len(), 1);
        assert!(m.setting("token").unwrap().secret);
        assert_eq!(m.setting("limit").unwrap().default_value(), 20);
    }

    #[test]
    fn version_gate() {
        assert!(version_gt("0.2.0", "0.1.0"));
        assert!(version_gt("1.0", "0.9.9"));
        assert!(!version_gt("0.1.0", "0.1.0"));
        assert!(!version_gt("0.1.0", "0.2.0"));
        let mut m = base();
        m.min_luvus_version = "99.0.0".into();
        assert!(m.validate().is_err(), "future requirement is refused");
    }

    #[test]
    fn duplicate_action_ids_rejected() {
        let mut m = base();
        for _ in 0..2 {
            m.actions.push(action("dup"));
        }
        assert!(m.validate().is_err());
    }
}
