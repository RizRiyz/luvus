//! User configuration at `~/.luvus/config.json` — theme, layout, notifications.
//! Loaded on startup and saved whenever Settings changes something. Every field
//! has a serde default, so old/new configs round-trip and a missing or corrupt
//! file just yields defaults.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::app::{SIDEBAR_WIDTH_DEFAULT, SIDEBAR_WIDTH_MAX, SIDEBAR_WIDTH_MIN};

const CONFIG_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub version: u32,
    #[serde(default = "default_theme")]
    pub theme: String,
    /// UI language code (docs/21) — `"en"` (default) or any `i18n::LANGS` code.
    #[serde(default = "default_lang")]
    pub language: String,
    /// Shell keyword for new panes (`default` / `powershell` / `cmd` / literal).
    #[serde(default = "default_shell_choice")]
    pub shell: String,
    /// Legacy single-sidebar width. Kept for back-compat + as the migration
    /// source for `sidebars`, and mirrored from `sidebars.left.width` on save so
    /// an older binary still finds a sensible width (docs/29).
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u16,
    /// Per-side sidebar layout (docs/29). `None` in a pre-DOCK config → migrated
    /// from `sidebar_width` into the default `[workspaces, agents]` left layout.
    #[serde(default)]
    pub sidebars: Option<SidebarsConfig>,
    #[serde(default)]
    pub layout: LayoutConfig,
    #[serde(default)]
    pub notifications: NotifyConfig,
    /// Check `luvus.dev/latest.json` in the background for a newer release and
    /// show an indicator by the version number. A single periodic `curl`/`wget`
    /// GET; on by default, toggled in Settings → General. Notify-only — luvus
    /// never self-updates (installed via cargo/brew/etc).
    #[serde(default = "yes")]
    pub check_updates: bool,
    /// Replay the CLI options an agent pane was launched with when resuming it
    /// after a restart (docs/62): a pane started as
    /// `claude --permission-mode … --model …` comes back with those options
    /// instead of a bare `claude --resume <id>`.
    ///
    /// One switch for the feature. The *options* are already per agent — each
    /// pane replays only what its own agent was launched with — so this is just
    /// whether that happens at all.
    ///
    /// **Off by default**: a remembered option outlives the session it was set
    /// for, and some of them widen what the agent may do without asking
    /// (`--permission-mode bypassPermissions`), so switching it on is deliberate.
    #[serde(default)]
    pub resume_launch_flags: bool,
    /// Custom keybindings: command id → key string (overrides the defaults).
    /// An empty value means the command is explicitly unbound.
    #[serde(default)]
    pub keybindings: std::collections::HashMap<String, String>,
    /// The safe prefix that opens command mode (docs/64): an F1-F12 key or a
    /// Ctrl/Alt character chord such as `ctrl+space`, `ctrl+b`, or `alt+\\`.
    /// Plain text keys are rejected so normal terminal typing is never swallowed.
    #[serde(default = "default_prefix")]
    pub prefix: String,
    /// Mission Control cost overrides (docs/54, MC-5): model-id substring →
    /// `[input, output, cache]` USD per **million** tokens, taking precedence over
    /// the built-in price table. Empty by default (use the bundled estimates).
    #[serde(default)]
    pub mission_pricing: std::collections::HashMap<String, [f64; 3]>,
    /// Mission Control cost budget in USD (docs/54): when a workspace's total
    /// session cost passes it, the header cost turns red. `None` = no budget.
    #[serde(default)]
    pub mission_budget: Option<f64>,
    /// Module dock ids the user has explicitly turned **off** in Settings → Layout
    /// (docs/29). A module re-pushes its dock on startup and on events (e.g. the
    /// esp-idf example on `workspace.created`), and auto-mount cannot otherwise
    /// tell "user turned it off" from "never placed yet" — both are "not on any
    /// side" — so it would resurrect the dock on its default side on the next push
    /// or restart. This set keeps an off dock off; re-placing it clears the flag.
    #[serde(default)]
    pub docks_off: Vec<String>,
    /// Luvus Bar placement groups. Dynamic content is never persisted here;
    /// only presentation preferences survive a restart.
    #[serde(default)]
    pub bars: BarConfig,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BarConfig {
    #[serde(default)]
    pub top_right: Vec<String>,
    #[serde(default = "default_bottom_bars")]
    pub bottom_right: Vec<String>,
    #[serde(default)]
    pub off: Vec<String>,
}

fn default_bottom_bars() -> Vec<String> {
    vec![crate::bar::CORE_RUNTIME.to_string()]
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            top_right: Vec::new(),
            bottom_right: default_bottom_bars(),
            off: Vec::new(),
        }
    }
}

impl BarConfig {
    pub fn order(&self, region: crate::bar::BarRegion) -> &[String] {
        match region {
            crate::bar::BarRegion::TopRight => &self.top_right,
            crate::bar::BarRegion::BottomRight => &self.bottom_right,
        }
    }

    pub fn region_for(
        &self,
        key: &str,
        fallback: crate::bar::BarRegion,
    ) -> Option<crate::bar::BarRegion> {
        if self.off.iter().any(|candidate| candidate == key) {
            None
        } else if self.top_right.iter().any(|candidate| candidate == key) {
            Some(crate::bar::BarRegion::TopRight)
        } else if self.bottom_right.iter().any(|candidate| candidate == key) {
            Some(crate::bar::BarRegion::BottomRight)
        } else {
            Some(fallback)
        }
    }

    /// Whether `key` already has exactly this persisted placement.
    ///
    /// This deliberately differs from [`Self::region_for`]: an undeclared
    /// preference can currently resolve to the requested fallback, but the
    /// first explicit move still needs to be saved so a later module-default
    /// change does not move the user's bar.
    pub fn is_explicitly_placed(&self, key: &str, region: Option<crate::bar::BarRegion>) -> bool {
        let top = self
            .top_right
            .iter()
            .filter(|candidate| candidate.as_str() == key)
            .count();
        let bottom = self
            .bottom_right
            .iter()
            .filter(|candidate| candidate.as_str() == key)
            .count();
        let off = self
            .off
            .iter()
            .filter(|candidate| candidate.as_str() == key)
            .count();
        match region {
            Some(crate::bar::BarRegion::TopRight) => top == 1 && bottom == 0 && off == 0,
            Some(crate::bar::BarRegion::BottomRight) => top == 0 && bottom == 1 && off == 0,
            None => top == 0 && bottom == 0 && off == 1,
        }
    }

    pub fn place(&mut self, key: &str, region: Option<crate::bar::BarRegion>) {
        self.top_right.retain(|candidate| candidate != key);
        self.bottom_right.retain(|candidate| candidate != key);
        self.off.retain(|candidate| candidate != key);
        match region {
            Some(crate::bar::BarRegion::TopRight) => self.top_right.push(key.to_string()),
            Some(crate::bar::BarRegion::BottomRight) => self.bottom_right.push(key.to_string()),
            None => self.off.push(key.to_string()),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LayoutConfig {
    #[serde(default = "one")]
    pub col_gap: u16,
    #[serde(default)]
    pub row_gap: u16,
    #[serde(default = "yes")]
    pub show_titles: bool,
    /// When a pane is named (`pane name` / `agent name`), also show its cwd path
    /// after the name in the title strip. Off by default: a named pane shows just
    /// its name, an unnamed pane its path (the original behavior).
    #[serde(default)]
    pub pane_title_path: bool,
    /// In the AGENTS sidebar, show each agent's live session title (the OSC title
    /// it sets, e.g. "Ship the desktop release…") in place of the `wsname · =<id>`
    /// meta line. Off by default; falls back to the meta line when an agent set no
    /// useful title.
    #[serde(default)]
    pub agent_title: bool,
    /// Resume a session into its own workspace (else a new tab in the current one).
    #[serde(default = "yes", alias = "resume_in_new_node")]
    pub resume_in_new_workspace: bool,
    /// Open a new tab/split at the workspace root instead of inheriting the
    /// focused pane's live cwd. Off by default: a new tab/split starts where the
    /// user is working; turn this on to always reset to the workspace root.
    #[serde(default)]
    pub new_pane_to_workspace_root: bool,
    /// Default action when a file is opened from the FILES tree (docs/38):
    /// `"readonly"` (the native viewer) or an editor run-command such as `"vim"`
    /// / `"emacs -nw"`. A plain click uses this; Shift+click always reads it
    /// read-only, and the right-click menu picks per file.
    #[serde(default = "default_file_open")]
    pub file_open: String,
    /// Retained scrollback budget per pane. This is the user-facing memory dial:
    /// 10 MiB by default, regardless of how many panes are open. The Alacritty
    /// adapter derives a conservative row limit from it until the Ghostty engine
    /// can enforce a native byte budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_bytes: Option<usize>,
    /// Pre-v0.10.3 line-count setting. Read for migration but never write again.
    #[serde(default = "default_scrollback", skip_serializing)]
    pub scrollback: usize,
    /// Show dotfiles in the FILES tree (docs/38). On by default (dev projects
    /// lean on `.env`/`.gitignore`/`.github` and hiding them surprised people);
    /// toggled in Settings → General, and `.git` is always hidden regardless.
    /// `default = "yes"` so an older config without the field also gets it on.
    /// Persisted so the choice sticks across restarts.
    #[serde(default = "yes")]
    pub files_show_hidden: bool,
    /// Native DIFF review display defaults (docs/88).
    #[serde(default)]
    pub diff_layout: crate::diff::DiffLayoutPreference,
    #[serde(default)]
    pub diff_wrap: bool,
    #[serde(default = "default_diff_context_lines")]
    pub diff_context_lines: u16,
    #[serde(default = "yes")]
    pub diff_show_line_numbers: bool,
    #[serde(default)]
    pub diff_marker_style: crate::diff::DiffMarkerStyle,
    /// Whether changed rows use semantic colors from the active theme or the
    /// familiar fixed red/green review palette.
    #[serde(default)]
    pub diff_color_mode: crate::diff::DiffColorMode,
    #[serde(default = "yes")]
    pub diff_live_refresh: bool,
    /// Terminal width (columns) below which the touch/compact layout kicks in
    /// (docs/18): one zoomed pane, sidebars hidden, the `≡` switcher. Configurable
    /// because phone terminals in landscape often sit right around the default;
    /// `0` disables compact mode entirely (the full UI always renders).
    #[serde(default = "default_compact_width")]
    pub compact_width: u16,
    /// What luvus forwards to a pane for **Shift/Alt+Enter** ("new line, don't
    /// submit"). A keyword from [`SHIFT_ENTER_CHOICES`]; default `esc-cr`
    /// (`ESC CR`, the sequence Claude Code's `/terminal-setup` installs). Exposed
    /// because agents/terminals disagree on which byte sequence they treat as a
    /// newline — notably some Windows agents want a bare `LF` where macOS wants
    /// `ESC CR`. Set once, applied to every pane's keystroke encoding.
    #[serde(default = "default_shift_enter")]
    pub shift_enter: String,
}

fn default_compact_width() -> u16 {
    crate::app::COMPACT_WIDTH
}

fn default_diff_context_lines() -> u16 {
    3
}

fn default_shift_enter() -> String {
    SHIFT_ENTER_CHOICES[0].0.to_string()
}

/// Ordered choices for what Shift/Alt+Enter sends to a pane: `(keyword, label,
/// bytes)`. The keyword is the stable `config.layout.shift_enter` value; the
/// label is shown in the Settings chooser; the bytes are what `encode_key`
/// forwards. `ESC CR` leads because it is what agent CLIs expect out of the box
/// (Claude Code's `/terminal-setup`). The others cover agents/terminals that
/// bind a plain `LF` or the CSI-u modified-Enter form instead.
pub const SHIFT_ENTER_CHOICES: &[(&str, &str, &[u8])] = &[
    ("esc-cr", "ESC CR (default)", b"\x1b\r"),
    ("lf", "LF (newline)", b"\n"),
    ("esc-lf", "ESC LF", b"\x1b\n"),
    ("csi-u", "CSI-u (\\e[13;2u)", b"\x1b[13;2u"),
];

/// Left + right sidebar layout (docs/29). Serialized under `sidebars`.
#[derive(Serialize, Deserialize, Clone)]
pub struct SidebarsConfig {
    #[serde(default = "SideConfig::left_default")]
    pub left: SideConfig,
    #[serde(default = "SideConfig::right_default")]
    pub right: SideConfig,
}

/// One sidebar's persisted state: shown/hidden, width, and its ordered dock ids.
#[derive(Serialize, Deserialize, Clone)]
pub struct SideConfig {
    #[serde(default = "yes")]
    pub visible: bool,
    #[serde(default = "default_sidebar_width")]
    pub width: u16,
    #[serde(default)]
    pub docks: Vec<String>,
}

impl SideConfig {
    /// The default left sidebar: shown, holding workspaces then agents.
    pub fn left_default() -> SideConfig {
        SideConfig {
            visible: true,
            width: SIDEBAR_WIDTH_DEFAULT,
            docks: vec!["workspaces".into(), "agents".into()],
        }
    }
    /// The default right sidebar: off and empty.
    pub fn right_default() -> SideConfig {
        SideConfig {
            visible: false,
            width: SIDEBAR_WIDTH_DEFAULT,
            docks: Vec::new(),
        }
    }
}

impl SidebarsConfig {
    /// Today's layout: left holds workspaces + agents, right is off.
    pub fn default_layout() -> SidebarsConfig {
        SidebarsConfig {
            left: SideConfig::left_default(),
            right: SideConfig::right_default(),
        }
    }
    /// Migrate a pre-DOCK config: the default layout at the stored width.
    pub fn migrate(width: u16) -> SidebarsConfig {
        let mut s = Self::default_layout();
        s.left.width = width;
        s
    }
}

/// Sound alerts. The retro chime is optional, so both default to **off** —
/// nothing rings until the user turns it on in Settings → General.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct NotifyConfig {
    /// Play the retro chime when an agent finishes a working stretch.
    #[serde(default)]
    pub sound_on_done: bool,
    /// Play the same chime when an agent blocks on a permission prompt.
    #[serde(default)]
    pub sound_on_blocked: bool,
}

fn default_theme() -> String {
    "quattro-rally".to_string()
}
fn default_lang() -> String {
    "en".to_string()
}
fn default_shell_choice() -> String {
    "default".to_string()
}
/// The file-viewer sentinel for "open read-only in the native viewer".
pub const FILE_OPEN_READONLY: &str = "readonly";
fn default_file_open() -> String {
    FILE_OPEN_READONLY.to_string()
}
fn default_sidebar_width() -> u16 {
    SIDEBAR_WIDTH_DEFAULT
}
fn one() -> u16 {
    1
}
fn yes() -> bool {
    true
}
fn default_prefix() -> String {
    "ctrl+space".to_string()
}
fn default_scrollback() -> usize {
    SCROLLBACK_DEFAULT
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            theme: default_theme(),
            language: default_lang(),
            shell: default_shell_choice(),
            sidebar_width: default_sidebar_width(),
            sidebars: None,
            layout: LayoutConfig::default(),
            notifications: NotifyConfig::default(),
            check_updates: true,
            resume_launch_flags: false,
            keybindings: std::collections::HashMap::new(),
            prefix: default_prefix(),
            mission_pricing: std::collections::HashMap::new(),
            mission_budget: None,
            docks_off: Vec::new(),
            bars: BarConfig::default(),
        }
    }
}

impl Default for LayoutConfig {
    fn default() -> Self {
        LayoutConfig {
            col_gap: 1,
            row_gap: 0,
            show_titles: true,
            pane_title_path: false,
            agent_title: false,
            resume_in_new_workspace: true,
            new_pane_to_workspace_root: false,
            file_open: default_file_open(),
            scrollback_bytes: Some(SCROLLBACK_BYTES_DEFAULT),
            scrollback: default_scrollback(),
            files_show_hidden: true,
            diff_layout: crate::diff::DiffLayoutPreference::Auto,
            diff_wrap: false,
            diff_context_lines: default_diff_context_lines(),
            diff_show_line_numbers: true,
            diff_marker_style: crate::diff::DiffMarkerStyle::Symbols,
            diff_color_mode: crate::diff::DiffColorMode::Theme,
            diff_live_refresh: true,
            compact_width: default_compact_width(),
            shift_enter: default_shift_enter(),
        }
    }
}

/// Legacy line-count defaults retained only to migrate existing config files.
pub const SCROLLBACK_DEFAULT: usize = 2_000;
pub const SCROLLBACK_MIN: usize = 200;
pub const SCROLLBACK_MAX: usize = 20_000;
/// One MiB in bytes. Kept explicit so Settings and config diagnostics use the
/// same unit without a dependency.
pub const MIB: usize = 1024 * 1024;
/// Default per-pane retained-history budget.
pub const SCROLLBACK_BYTES_DEFAULT: usize = 10 * MIB;
pub const SCROLLBACK_BYTES_MIN: usize = MIB;
pub const SCROLLBACK_BYTES_MAX: usize = 256 * MIB;
/// Settings changes history memory one MiB at a time.
pub const SCROLLBACK_BYTES_STEP: usize = MIB;

impl Config {
    /// Per-pane history memory budget, clamped to a safe range. Existing
    /// line-count config is converted once on load; the fallback here keeps
    /// direct deserialization and old plugins safe as well.
    pub fn scrollback_bytes(&self) -> usize {
        self.layout
            .scrollback_bytes
            .unwrap_or_else(|| legacy_scrollback_bytes(self.layout.scrollback))
            .clamp(SCROLLBACK_BYTES_MIN, SCROLLBACK_BYTES_MAX)
    }

    /// Bytes forwarded to a pane for Shift/Alt+Enter (see `shift_enter`). Falls
    /// back to the default (`ESC CR`) if the stored keyword is unrecognized.
    pub fn shift_enter_bytes(&self) -> &'static [u8] {
        SHIFT_ENTER_CHOICES
            .iter()
            .find(|(k, _, _)| *k == self.layout.shift_enter)
            .map(|(_, _, b)| *b)
            .unwrap_or(SHIFT_ENTER_CHOICES[0].2)
    }

    /// Git context lines used by native DIFF reads. Hand-edited configuration
    /// is bounded here as well as on load so no caller can construct an
    /// unbounded `git diff --unified` argument.
    pub fn diff_context_lines(&self) -> u16 {
        self.layout
            .diff_context_lines
            .min(crate::diff::MAX_CONTEXT_LINES)
    }

    /// Clamp the persisted sidebar width into the supported range.
    pub fn sidebar_width(&self) -> u16 {
        self.sidebar_width
            .clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX)
    }

    /// Resolved sidebar layout: the stored `sidebars`, or a migration from the
    /// legacy `sidebar_width` reproducing today's default layout (docs/29).
    pub fn sidebars(&self) -> SidebarsConfig {
        self.sidebars
            .clone()
            .unwrap_or_else(|| SidebarsConfig::migrate(self.sidebar_width()))
    }
}

fn config_path() -> PathBuf {
    crate::persist::config_dir().join("config.json")
}

/// Load the config, or defaults if missing / unparsable.
pub fn load() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .map(normalize_config)
        .unwrap_or_default()
}

/// Hydrate an old line-count setting into the new persisted byte budget. The
/// old default becomes today's 10 MiB default; custom values retain their rough
/// relative size using the previous measured 5,000 lines at 120 columns ≈ 10
/// MiB relationship.
pub(crate) fn normalize_config(mut cfg: Config) -> Config {
    if cfg.layout.scrollback_bytes.is_none() {
        cfg.layout.scrollback_bytes = Some(legacy_scrollback_bytes(cfg.layout.scrollback));
    }
    cfg.layout.diff_context_lines = cfg
        .layout
        .diff_context_lines
        .min(crate::diff::MAX_CONTEXT_LINES);
    cfg
}

fn legacy_scrollback_bytes(lines: usize) -> usize {
    if lines == SCROLLBACK_DEFAULT {
        return SCROLLBACK_BYTES_DEFAULT;
    }
    lines
        .clamp(SCROLLBACK_MIN, SCROLLBACK_MAX)
        .saturating_mul(SCROLLBACK_BYTES_DEFAULT)
        .saturating_div(5_000)
        .clamp(SCROLLBACK_BYTES_MIN, SCROLLBACK_BYTES_MAX)
}

/// Save the config atomically (best effort).
pub fn save(cfg: &Config) {
    let dir = crate::persist::ensure_config_dir();
    if !dir.is_dir() {
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(cfg) else {
        return;
    };
    let path = config_path();
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = fs::File::create(&tmp) {
        if f.write_all(json.as_bytes()).is_ok() && f.flush().is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_roundtrip() {
        let c = Config::default();
        assert_eq!(c.theme, "quattro-rally");
        assert!(c.layout.show_titles);
        assert_eq!(c.layout.col_gap, 1);
        // Empty object → all defaults (forward/back compat).
        let from_empty: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(from_empty.theme, "quattro-rally");
        assert_eq!(from_empty.sidebar_width, SIDEBAR_WIDTH_DEFAULT);
        assert_eq!(
            from_empty.bars.bottom_right,
            vec![crate::bar::CORE_RUNTIME.to_string()],
            "old configs gain the default runtime bar"
        );
        assert!(from_empty.bars.top_right.is_empty());
        // Round-trip preserves values.
        // Scrollback defaults to a per-pane 10 MiB budget. The legacy line
        // field remains only so old config can migrate safely.
        assert_eq!(c.scrollback_bytes(), SCROLLBACK_BYTES_DEFAULT);
        let mut wild = Config::default();
        wild.layout.scrollback_bytes = Some(usize::MAX);
        assert_eq!(
            wild.scrollback_bytes(),
            SCROLLBACK_BYTES_MAX,
            "absurd values clamp down"
        );
        wild.layout.scrollback_bytes = Some(1);
        assert_eq!(
            wild.scrollback_bytes(),
            SCROLLBACK_BYTES_MIN,
            "tiny values clamp up"
        );
        // An old config written before this field still loads, at the new default.
        let old: Config = serde_json::from_str(r#"{"layout":{"col_gap":1}}"#).unwrap();
        assert_eq!(old.scrollback_bytes(), SCROLLBACK_BYTES_DEFAULT);
        let old_custom: Config = serde_json::from_str(r#"{"layout":{"scrollback":5000}}"#).unwrap();
        assert_eq!(old_custom.scrollback_bytes(), SCROLLBACK_BYTES_DEFAULT);

        // Sounds are optional and must default to off.
        assert!(!c.notifications.sound_on_done);
        assert!(!c.notifications.sound_on_blocked);
        let c2 = Config {
            theme: "mono".into(),
            notifications: NotifyConfig {
                sound_on_done: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&c2).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme, "mono");
        assert!(back.notifications.sound_on_done);
        assert!(!back.notifications.sound_on_blocked);
    }

    #[test]
    fn explicit_bar_placement_is_distinct_from_a_default_fallback() {
        let mut bars = BarConfig {
            top_right: Vec::new(),
            bottom_right: Vec::new(),
            off: Vec::new(),
        };
        let key = "you.ci:status";

        assert_eq!(
            bars.region_for(key, crate::bar::BarRegion::TopRight),
            Some(crate::bar::BarRegion::TopRight)
        );
        assert!(
            !bars.is_explicitly_placed(key, Some(crate::bar::BarRegion::TopRight)),
            "an effective default is not yet a persisted user preference"
        );

        bars.place(key, Some(crate::bar::BarRegion::TopRight));
        assert!(bars.is_explicitly_placed(key, Some(crate::bar::BarRegion::TopRight)));

        // A malformed duplicate is not treated as idempotent: the next place
        // operation should be allowed to normalize it back to one entry.
        bars.top_right.push(key.to_string());
        assert!(!bars.is_explicitly_placed(key, Some(crate::bar::BarRegion::TopRight)));
    }
}
