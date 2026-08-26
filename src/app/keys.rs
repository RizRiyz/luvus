//! Keybindings — the prefix-mode command registry. After the configured prefix, a key
//! triggers a [`Cmd`]. Defaults are listed here; users can rebind any command to
//! a different key in Settings → Keys (persisted to `config.keybindings`). A few
//! fixed aliases (vim `hjkl`, `Tab`/`⇧Tab`, `q`) are always available too.

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

/// Is this a real `Ctrl` chord, or is it AltGr typing a character?
///
/// The Windows console reports AltGr as `CONTROL | ALT` — the layout driver
/// presses left-Ctrl together with right-Alt — so anything that only asks
/// `modifiers.contains(CONTROL)` swallows every AltGr character. On the Spanish
/// layout that is `\ @ # [ ] { } | ~ €`: typing a Windows path into a pane sent
/// `0x1c` instead of `\`, and `[` sent a bare `ESC`.
///
/// Only Windows needs the distinction: X11/macOS terminals deliver an AltGr
/// character with no modifiers at all, and there `Ctrl+Alt+<key>` really is a
/// chord the user meant, so it must keep working.
pub fn is_ctrl_chord(mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::CONTROL) && !(cfg!(windows) && mods.contains(KeyModifiers::ALT))
}

/// A prefix-mode command — the thing a key triggers after `Ctrl+Space`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    NextPane,
    NextAttention,
    SplitRight,
    SplitDown,
    ForkSession,
    ClosePane,
    ZoomPane,
    ResizeMode,
    CopyMode,
    NewTab,
    NextTab,
    PrevTab,
    RenameTab,
    NewWorkspace,
    CloseWorkspace,
    NextWorkspace,
    PrevWorkspace,
    NewWorktree,
    OpenGit,
    OpenBoard,
    OpenSettings,
    ToggleSidebar,
    ToggleRightSidebar,
    ToggleAgents,
    ToggleFiles,
    Switcher,
    GlobalSearch,
    Detach,
}

impl Cmd {
    /// Every command, grouped for the Settings → Keys list.
    pub const ALL: &'static [Cmd] = &[
        Cmd::FocusLeft,
        Cmd::FocusDown,
        Cmd::FocusUp,
        Cmd::FocusRight,
        Cmd::NextPane,
        Cmd::NextAttention,
        Cmd::SplitRight,
        Cmd::SplitDown,
        Cmd::ForkSession,
        Cmd::ClosePane,
        Cmd::ZoomPane,
        Cmd::ResizeMode,
        Cmd::CopyMode,
        Cmd::NewTab,
        Cmd::NextTab,
        Cmd::PrevTab,
        Cmd::RenameTab,
        Cmd::NewWorkspace,
        Cmd::CloseWorkspace,
        Cmd::NextWorkspace,
        Cmd::PrevWorkspace,
        Cmd::NewWorktree,
        Cmd::OpenGit,
        Cmd::OpenBoard,
        Cmd::OpenSettings,
        Cmd::ToggleSidebar,
        Cmd::ToggleRightSidebar,
        Cmd::ToggleAgents,
        Cmd::ToggleFiles,
        Cmd::Switcher,
        Cmd::GlobalSearch,
        Cmd::Detach,
    ];

    /// Stable id used as the config key (must never change once shipped).
    pub fn id(self) -> &'static str {
        match self {
            Cmd::FocusLeft => "focus_left",
            Cmd::FocusDown => "focus_down",
            Cmd::FocusUp => "focus_up",
            Cmd::FocusRight => "focus_right",
            Cmd::NextPane => "next_pane",
            Cmd::NextAttention => "next_attention",
            Cmd::SplitRight => "split_right",
            Cmd::SplitDown => "split_down",
            Cmd::ForkSession => "fork_session",
            Cmd::ClosePane => "close_pane",
            Cmd::ZoomPane => "zoom_pane",
            Cmd::ResizeMode => "resize_mode",
            Cmd::CopyMode => "copy_mode",
            Cmd::NewTab => "new_tab",
            Cmd::NextTab => "next_tab",
            Cmd::PrevTab => "prev_tab",
            Cmd::RenameTab => "rename_tab",
            Cmd::NewWorkspace => "new_node",
            Cmd::CloseWorkspace => "close_node",
            Cmd::NextWorkspace => "next_node",
            Cmd::PrevWorkspace => "prev_node",
            Cmd::NewWorktree => "new_worktree",
            Cmd::OpenGit => "open_git",
            Cmd::OpenBoard => "open_board",
            Cmd::OpenSettings => "open_settings",
            Cmd::ToggleSidebar => "toggle_sidebar",
            Cmd::ToggleRightSidebar => "toggle_right_sidebar",
            Cmd::ToggleAgents => "toggle_agents",
            Cmd::ToggleFiles => "toggle_files",
            Cmd::Switcher => "switcher",
            Cmd::GlobalSearch => "search",
            Cmd::Detach => "detach",
        }
    }

    /// Human label shown in the Keys list / cheat-sheet, in the active language
    /// (docs/21). `id()` stays the stable English key; only this display label
    /// is localized.
    pub fn label(self, cat: &crate::i18n::Catalog) -> &'static str {
        match self {
            Cmd::FocusLeft => cat.cmd_focus_left,
            Cmd::FocusDown => cat.cmd_focus_down,
            Cmd::FocusUp => cat.cmd_focus_up,
            Cmd::FocusRight => cat.cmd_focus_right,
            Cmd::NextPane => cat.cmd_next_pane,
            Cmd::NextAttention => cat.cmd_next_attention,
            Cmd::SplitRight => cat.cmd_split_right,
            Cmd::SplitDown => cat.cmd_split_down,
            Cmd::ForkSession => cat.cmd_fork_session,
            Cmd::ClosePane => cat.cmd_close_pane,
            Cmd::ZoomPane => cat.cmd_zoom_pane,
            Cmd::ResizeMode => cat.cmd_resize_mode,
            Cmd::CopyMode => cat.settings.keys_copy_terminal_text,
            Cmd::NewTab => cat.cmd_new_tab,
            Cmd::NextTab => cat.cmd_next_tab,
            Cmd::PrevTab => cat.cmd_prev_tab,
            Cmd::RenameTab => cat.cmd_rename_tab,
            Cmd::NewWorkspace => cat.cmd_new_workspace,
            Cmd::CloseWorkspace => cat.cmd_close_workspace,
            Cmd::NextWorkspace => cat.cmd_next_workspace,
            Cmd::PrevWorkspace => cat.cmd_prev_workspace,
            Cmd::NewWorktree => cat.cmd_new_worktree,
            Cmd::OpenGit => cat.cmd_open_git,
            Cmd::OpenBoard => cat.cmd_open_board,
            Cmd::OpenSettings => cat.cmd_open_settings,
            Cmd::ToggleSidebar => cat.cmd_toggle_sidebar,
            Cmd::ToggleRightSidebar => cat.cmd_toggle_right_sidebar,
            Cmd::ToggleAgents => cat.cmd_toggle_agents,
            Cmd::ToggleFiles => cat.cmd_toggle_files,
            Cmd::Switcher => cat.cmd_switcher,
            Cmd::GlobalSearch => cat.cmd_search,
            Cmd::Detach => cat.cmd_detach,
        }
    }

    /// Localized group heading for the Settings → Keys list. `Cmd::ALL` is
    /// ordered so each group is contiguous.
    pub fn section(self, cat: &crate::i18n::Catalog) -> &'static str {
        match self {
            Cmd::FocusLeft
            | Cmd::FocusDown
            | Cmd::FocusUp
            | Cmd::FocusRight
            | Cmd::NextPane
            | Cmd::NextAttention
            | Cmd::SplitRight
            | Cmd::SplitDown
            | Cmd::ForkSession
            | Cmd::ClosePane
            | Cmd::ZoomPane
            | Cmd::ResizeMode
            | Cmd::CopyMode => cat.settings.keys_sections[0],
            Cmd::NewTab | Cmd::NextTab | Cmd::PrevTab | Cmd::RenameTab => {
                cat.settings.keys_sections[1]
            }
            Cmd::NewWorkspace
            | Cmd::CloseWorkspace
            | Cmd::NextWorkspace
            | Cmd::PrevWorkspace
            | Cmd::NewWorktree => cat.settings.keys_sections[2],
            Cmd::OpenGit
            | Cmd::OpenBoard
            | Cmd::OpenSettings
            | Cmd::ToggleSidebar
            | Cmd::ToggleRightSidebar
            | Cmd::ToggleAgents
            | Cmd::ToggleFiles
            | Cmd::GlobalSearch => cat.settings.keys_sections[3],
            Cmd::Switcher | Cmd::Detach => cat.settings.keys_sections[4],
        }
    }

    /// Default key (a [`key_string`] value).
    pub fn default_key(self) -> &'static str {
        match self {
            Cmd::FocusLeft => "←",
            Cmd::FocusDown => "↓",
            Cmd::FocusUp => "↑",
            Cmd::FocusRight => "→",
            Cmd::NextPane => ";",
            Cmd::NextAttention => ".",
            Cmd::SplitRight => "v",
            Cmd::SplitDown => "s",
            Cmd::ForkSession => "f",
            Cmd::ClosePane => "x",
            Cmd::ZoomPane => "z",
            Cmd::ResizeMode => "r",
            Cmd::CopyMode => "y",
            Cmd::NewTab => "c",
            Cmd::NextTab => "n",
            Cmd::PrevTab => "p",
            // `,` renames the tab on both luvus and tmux (tmux's rename-window).
            Cmd::RenameTab => ",",
            Cmd::NewWorkspace => "N",
            Cmd::CloseWorkspace => "D",
            Cmd::NextWorkspace => "w",
            Cmd::PrevWorkspace => "W",
            Cmd::NewWorktree => "G",
            Cmd::OpenGit => "g",
            Cmd::OpenBoard => "o",
            // `=` opens Settings (`,` now renames the tab, matching tmux). The
            // Menu button is always available too, so this is just the shortcut.
            Cmd::OpenSettings => "=",
            Cmd::ToggleSidebar => "b",
            Cmd::ToggleRightSidebar => "B",
            Cmd::ToggleAgents => "a",
            Cmd::ToggleFiles => "e",
            Cmd::Switcher => "m",
            Cmd::GlobalSearch => "/",
            Cmd::Detach => "d",
        }
    }

    /// Every default key for this command: the primary [`default_key`] plus any
    /// fixed aliases (vim `hjkl`, `Tab`/`⇧Tab`, `q`, `X`, `-`). Declaring the
    /// aliases here (docs/64) is what removes the hidden hardcoded alias block
    /// from `build_keymap` — every binding now flows through this list or a user
    /// override, so nothing is bound behind the user's back.
    pub fn default_keys(self) -> Vec<&'static str> {
        let mut keys = vec![self.default_key()];
        let aliases: &[&str] = match self {
            Cmd::FocusLeft => &["h"],
            Cmd::FocusDown => &["j"],
            Cmd::FocusUp => &["k"],
            Cmd::FocusRight => &["l"],
            Cmd::Detach => &["q"],
            Cmd::ClosePane => &["X"],
            Cmd::SplitDown => &["-"],
            Cmd::NextTab => &["⇥"],
            Cmd::PrevTab => &["⇧⇥"],
            _ => &[],
        };
        keys.extend_from_slice(aliases);
        keys
    }
}

/// Total reference rows (not counting the section headings) — the authoritative
/// count for the Keys-tab cursor, which steps through commands then these.
pub fn key_reference_rows() -> usize {
    crate::i18n::settings::KEY_REFERENCE_KEYS
        .iter()
        .map(|rows| rows.len())
        .sum()
}

/// Canonical string for a command key after the prefix has been consumed.
/// Used both to match presses and to display/store bindings.
pub fn key_string(key: &KeyEvent) -> Option<String> {
    Some(match key.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Tab => "⇥".into(),
        KeyCode::BackTab => "⇧⇥".into(),
        _ => return None,
    })
}

/// The prefix chord that opens command mode (docs/64). Safe single-key
/// prefixes are F1-F12. Character/Space prefixes must carry Ctrl or Alt so a
/// plain typed key can never disappear into command mode. Shift is supported
/// on function keys, where terminals can preserve it reliably.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixSpec {
    modifiers: KeyModifiers,
    code: KeyCode,
}

impl Default for PrefixSpec {
    fn default() -> Self {
        Self {
            modifiers: KeyModifiers::CONTROL,
            code: KeyCode::Char(' '),
        }
    }
}

impl PrefixSpec {
    const RELEVANT_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL
        .union(KeyModifiers::ALT)
        .union(KeyModifiers::SHIFT);

    /// Parse canonical specs such as `ctrl+space`, `alt+\`, `shift+f12`, or
    /// plain `f12`. Bare text remains invalid because it would swallow typing.
    pub fn parse(s: &str) -> Option<Self> {
        let mut modifiers = KeyModifiers::NONE;
        let mut code = None;
        for raw in s.split('+') {
            let part = raw.trim().to_ascii_lowercase();
            match part.as_str() {
                "" => {}
                "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
                // `meta` was historically treated as an Alt alias in config.
                "alt" | "option" | "opt" | "meta" => modifiers.insert(KeyModifiers::ALT),
                "shift" => modifiers.insert(KeyModifiers::SHIFT),
                "space" | "spc" if code.is_none() => code = Some(KeyCode::Char(' ')),
                "plus" if code.is_none() => code = Some(KeyCode::Char('+')),
                other if code.is_none() => {
                    if let Some(number) = other
                        .strip_prefix('f')
                        .and_then(|value| value.parse::<u8>().ok())
                        .filter(|number| (1..=12).contains(number))
                    {
                        code = Some(KeyCode::F(number));
                    } else if other.chars().count() == 1 && other.is_ascii() {
                        code = Some(KeyCode::Char(other.chars().next()?));
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        let code = code?;
        let is_function = matches!(code, KeyCode::F(1..=12));
        let carries_non_text_modifier =
            modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if !is_function && (!carries_non_text_modifier || modifiers.contains(KeyModifiers::SHIFT)) {
            return None;
        }
        Some(Self { modifiers, code })
    }

    /// Canonical persisted representation.
    pub fn spec(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("ctrl".to_string());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("alt".to_string());
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("shift".to_string());
        }
        parts.push(match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char('+') => "plus".to_string(),
            KeyCode::Char(character) => character.to_ascii_lowercase().to_string(),
            KeyCode::F(number) => format!("f{number}"),
            _ => unreachable!("PrefixSpec only stores supported keys"),
        });
        parts.join("+")
    }

    /// The normalized event used when a double prefix forwards the literal key
    /// through the same PTY encoder as ordinary input.
    pub fn key_event(&self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }

    /// Match the exact configured chord. Ctrl+Space keeps its NUL/Ctrl+@ legacy
    /// compatibility because those encodings cannot carry complete modifiers.
    pub fn matches(&self, key: &KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
        {
            return false;
        }
        if self.code == KeyCode::Char(' ') && self.modifiers == KeyModifiers::CONTROL {
            if key.code == KeyCode::Null {
                return key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL;
            }
            let modifiers = key.modifiers & Self::RELEVANT_MODIFIERS;
            if matches!(key.code, KeyCode::Char(' ')) {
                return modifiers == KeyModifiers::CONTROL;
            }
            // Some terminals spell Ctrl+Space as Ctrl+@ and may retain the
            // physical Shift needed to type `@`; both encodings are NUL.
            if matches!(key.code, KeyCode::Char('@')) {
                return modifiers == KeyModifiers::CONTROL
                    || modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT);
            }
        }
        if key.modifiers & Self::RELEVANT_MODIFIERS != self.modifiers {
            return false;
        }
        match (self.code, key.code) {
            (KeyCode::Char(expected), KeyCode::Char(actual)) => {
                expected.eq_ignore_ascii_case(&actual)
            }
            (expected, actual) => expected == actual,
        }
    }

    /// Human label used by Settings and the status line.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".to_string());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("Alt".to_string());
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("Shift".to_string());
        }
        parts.push(match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(character) => character.to_ascii_uppercase().to_string(),
            KeyCode::F(number) => format!("F{number}"),
            _ => unreachable!("PrefixSpec only stores supported keys"),
        });
        parts.join("+")
    }
}

/// Build the active `key → Cmd` map from the (id → key) config overrides on top
/// of every command's [`Cmd::default_keys`] (docs/64). There is no separate
/// hardcoded alias block: aliases are just extra default keys, so a user who
/// rebinds a command replaces its whole default set, and one who leaves it alone
/// gets the vim/Tab aliases exactly as before.
pub fn build_keymap(overrides: &HashMap<String, String>) -> HashMap<String, Cmd> {
    let mut m = HashMap::new();
    // 1) Explicit overrides first — they always win over any default.
    for &cmd in Cmd::ALL {
        if let Some(k) = overrides.get(cmd.id()) {
            if !k.is_empty() {
                m.insert(k.clone(), cmd);
            }
            // An empty override means "explicitly unbound": it claims nothing and
            // (via the `contains_key` check below) suppresses this command's
            // defaults too.
        }
    }
    // 2) Defaults fill any slot an override hasn't claimed, but only for commands
    //    the user hasn't rebound or unbound.
    for &cmd in Cmd::ALL {
        if overrides.contains_key(cmd.id()) {
            continue;
        }
        for k in cmd.default_keys() {
            m.entry(k.to_string()).or_insert(cmd);
        }
    }
    m
}

/// A named keybinding preset (docs/64): a prefix chord plus a set of command
/// overrides. Applying one is a batch of the same edits Settings → Keys makes by
/// hand, so a user can still tweak individual keys afterward.
pub struct Preset {
    /// Stable id used by config / the Settings chooser (`"default"`, `"tmux"`).
    pub id: &'static str,
    /// Human label for the Settings chooser.
    pub label: &'static str,
    /// The prefix spec this preset sets (`"ctrl+space"`, `"ctrl+b"`).
    pub prefix: &'static str,
    /// Command overrides (`cmd_id → key`). Anything absent keeps its default key.
    pub binds: &'static [(&'static str, &'static str)],
}

impl Preset {
    pub fn localized_label(&self, cat: &crate::i18n::Catalog) -> &'static str {
        match self.id {
            "default" => cat.settings.preset_default,
            "function" => cat.settings.preset_function,
            "tmux" => cat.settings.preset_tmux,
            _ => self.label,
        }
    }
}

/// The built-in presets. `default` restores luvus's own keys; `tmux` matches the
/// muscle memory of a tmux user (`Ctrl+b` prefix, `%`/`"` splits) - most other
/// tmux keys (`c`/`n`/`p`/`x`/`z`/`d`) already agree with luvus's defaults.
pub fn presets() -> &'static [Preset] {
    &[
        Preset {
            id: "default",
            label: "luvus (default)",
            prefix: "ctrl+space",
            binds: &[],
        },
        Preset {
            id: "function",
            label: "no-Ctrl (F12)",
            prefix: "f12",
            binds: &[],
        },
        Preset {
            id: "tmux",
            label: "tmux",
            prefix: "ctrl+b",
            binds: &[
                // tmux splits: `%` = left/right, `"` = top/bottom.
                ("split_right", "%"),
                ("split_down", "\""),
                // `o` cycles to the next pane (rename is `,` by default already).
                ("next_pane", "o"),
                // `(` / `)` step to the previous / next session (luvus workspace).
                ("prev_node", "("),
                ("next_node", ")"),
                // `w` opens the jump palette (tmux's choose-window / -tree); the
                // scope chips inside narrow it to tabs, workspaces, or agents.
                ("switcher", "w"),
            ],
        },
    ]
}

impl App {
    /// The key currently bound to `cmd` (override or default), for display.
    pub fn key_for(&self, cmd: Cmd) -> String {
        self.config
            .keybindings
            .get(cmd.id())
            .cloned()
            .unwrap_or_else(|| cmd.default_key().to_string())
    }

    /// Rebind `cmd` to `key`, persist, and rebuild the active keymap. If `key`
    /// was used by another command, that one is cleared (so it can be rebound).
    pub fn rebind(&mut self, cmd: Cmd, key: String) {
        // Steal the key only from other commands that *explicitly* bound it, so we
        // never leave two overrides fighting for one key. A command that merely
        // has it as a default yields on its own in `build_keymap` (or_insert).
        let others: Vec<String> = self
            .config
            .keybindings
            .iter()
            .filter(|(id, k)| id.as_str() != cmd.id() && !k.is_empty() && **k == key)
            .map(|(id, _)| id.clone())
            .collect();
        for id in others {
            self.config.keybindings.insert(id, String::new());
        }
        self.config.keybindings.insert(cmd.id().to_string(), key);
        self.keymap = build_keymap(&self.config.keybindings);
        crate::config::save(&self.config);
    }

    /// Reset `cmd` to its default key (drop any override), persist, and rebuild.
    pub fn reset_binding(&mut self, cmd: Cmd) {
        self.config.keybindings.remove(cmd.id());
        self.keymap = build_keymap(&self.config.keybindings);
        crate::config::save(&self.config);
    }

    /// Set the command-mode prefix from a safe spec such as `ctrl+b`, `alt+\`,
    /// or `f12` (docs/64). Plain text keys are rejected so normal typing cannot
    /// disappear into command mode. On success it applies live and persists.
    pub fn set_prefix(&mut self, spec: &str) -> bool {
        match PrefixSpec::parse(spec) {
            Some(p) => {
                self.config.prefix = p.spec();
                self.prefix = p;
                crate::config::save(&self.config);
                true
            }
            None => false,
        }
    }

    /// Apply a named keybinding preset (docs/64): set the prefix and replace the
    /// keybinding overrides with the preset's, then rebuild + persist. `"default"`
    /// clears every override back to luvus's built-in keys. Returns `false` for an
    /// unknown preset name.
    pub fn apply_preset(&mut self, name: &str) -> bool {
        let Some(preset) = presets().iter().find(|p| p.id == name) else {
            return false;
        };
        self.config.keybindings.clear();
        for (id, key) in preset.binds {
            self.config
                .keybindings
                .insert((*id).to_string(), (*key).to_string());
        }
        self.set_prefix(preset.prefix); // also saves config
        self.keymap = build_keymap(&self.config.keybindings);
        crate::config::save(&self.config);
        true
    }

    /// Run a prefix command.
    pub fn run_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::FocusLeft => self.focus_dir(Dir::Left),
            Cmd::FocusDown => self.focus_dir(Dir::Down),
            Cmd::FocusUp => self.focus_dir(Dir::Up),
            Cmd::FocusRight => self.focus_dir(Dir::Right),
            Cmd::NextPane => self.focus_next_pane(),
            Cmd::NextAttention => self.focus_next_attention(),
            Cmd::SplitRight => self.split(Axis::Col),
            Cmd::SplitDown => self.split(Axis::Row),
            // Fork the focused agent pane's session into a new pane (no-op if it
            // isn't a fork-capable agent).
            Cmd::ForkSession => {
                self.fork_pane(self.layout().focus);
            }
            Cmd::ClosePane => {
                // A git tab / orchestration board has no real pane — close the
                // dashboard tab instead.
                if self.active_is_git() {
                    self.close_git_tab();
                } else if self.active_is_orch() {
                    self.close_orch_board();
                } else if self.active_is_mission() {
                    self.close_mission_tab();
                } else {
                    self.close_pane(self.layout().focus);
                }
            }
            Cmd::ZoomPane => self.zoomed = !self.zoomed,
            Cmd::ResizeMode => self.enter_resize_mode(),
            Cmd::CopyMode => {
                self.begin_copy_mode();
            }
            Cmd::NewTab => self.new_tab(),
            Cmd::NextTab => self.cycle_tab(1),
            Cmd::PrevTab => self.cycle_tab(-1),
            Cmd::RenameTab => {
                let i = self.ws().active_tab;
                self.open_tab_rename(i);
            }
            Cmd::NewWorkspace => self.open_folder_picker(),
            Cmd::CloseWorkspace => {
                let i = self.active_ws;
                self.close_workspace(i);
            }
            Cmd::NextWorkspace => self.cycle_workspace(1),
            Cmd::PrevWorkspace => self.cycle_workspace(-1),
            Cmd::NewWorktree => self.open_worktree_prompt(),
            Cmd::OpenGit => self.open_git_tab_active(),
            Cmd::OpenBoard => self.open_orch_board(),
            Cmd::OpenSettings => self.open_settings(),
            Cmd::ToggleSidebar => self.toggle_all_sides(),
            Cmd::ToggleRightSidebar => self.toggle_side(crate::app::Side::Right),
            Cmd::ToggleAgents => self.agents_active_only = !self.agents_active_only,
            Cmd::ToggleFiles => self.toggle_files_dock(),
            Cmd::Switcher => self.toggle_switcher(),
            Cmd::GlobalSearch => self.toggle_search(),
            Cmd::Detach => self.detach_requested = true,
        }
    }

    /// Jump focus to the next agent pane that is **Blocked** — one waiting on the
    /// user — cycling in the same node → tab → pane order the AGENTS sidebar lists
    /// (QW-1, docs/46). Crosses nodes and tabs via [`focus_pane_global`]. With
    /// nothing waiting it flashes a toast instead of moving focus, so the key is
    /// always safe to mash.
    pub fn focus_next_attention(&mut self) {
        let mut blocked: Vec<crate::ids::PaneId> = Vec::new();
        for ws in &self.workspaces {
            for tab in &ws.tabs {
                for id in tab.layout.leaves() {
                    if let Some(s) = self.status.get(&id) {
                        let is_agent =
                            self.manifests.is_agent(&s.agent) || s.agent_session.is_some();
                        if is_agent && s.state == crate::ui::theme::State::Blocked {
                            blocked.push(id);
                        }
                    }
                }
            }
        }
        if blocked.is_empty() {
            let msg = self.catalog.no_agents_waiting;
            self.show_toast(msg);
            return;
        }
        // Advance from the current focus if it's already on a waiting agent,
        // otherwise start at the first one.
        let focus = self.layout().focus;
        let next = match blocked.iter().position(|&b| b == focus) {
            Some(i) => blocked[(i + 1) % blocked.len()],
            None => blocked[0],
        };
        self.focus_pane_global(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_aliases_resolve() {
        let m = build_keymap(&HashMap::new());
        assert_eq!(m.get("←"), Some(&Cmd::FocusLeft));
        assert_eq!(m.get("h"), Some(&Cmd::FocusLeft)); // vim alias
        assert_eq!(m.get("⇥"), Some(&Cmd::NextTab));
        assert_eq!(m.get("N"), Some(&Cmd::NewWorkspace));
        // `,` renames the tab (tmux-compatible); Settings moved to `=`.
        assert_eq!(m.get(","), Some(&Cmd::RenameTab));
        assert_eq!(m.get("="), Some(&Cmd::OpenSettings));
        assert_eq!(m.get("y"), Some(&Cmd::CopyMode));
        // every command is reachable by its default key
        for &c in Cmd::ALL {
            assert!(m.values().any(|v| *v == c), "{c:?} bound");
        }
    }

    #[test]
    fn rebind_moves_the_key() {
        let mut o = HashMap::new();
        o.insert(Cmd::NewTab.id().to_string(), "t".to_string());
        let m = build_keymap(&o);
        assert_eq!(m.get("t"), Some(&Cmd::NewTab));
        assert_ne!(m.get("c"), Some(&Cmd::NewTab)); // old default freed
    }

    #[test]
    fn copy_mode_binding_is_editable_like_every_other_prefix_command() {
        let _env = crate::persist::test_env("copy-mode-rebind");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        assert_eq!(app.key_for(Cmd::CopyMode), "y");
        app.rebind(Cmd::CopyMode, "t".into());
        assert_eq!(app.key_for(Cmd::CopyMode), "t");
        assert_eq!(app.keymap.get("t"), Some(&Cmd::CopyMode));

        app.reset_binding(Cmd::CopyMode);
        assert_eq!(app.key_for(Cmd::CopyMode), "y");
        assert_eq!(app.keymap.get("y"), Some(&Cmd::CopyMode));
    }

    #[test]
    fn rebinding_a_command_replaces_only_its_own_keys() {
        // Overriding FocusLeft to `g` drops its default set (← and h) and binds g.
        let mut o = HashMap::new();
        o.insert(Cmd::FocusLeft.id().to_string(), "g".to_string());
        let m = build_keymap(&o);
        assert_eq!(m.get("g"), Some(&Cmd::FocusLeft));
        assert_eq!(m.get("←"), None, "old primary freed");
        assert_eq!(m.get("h"), None, "old alias freed too");
        // Other commands' aliases are untouched.
        assert_eq!(m.get("j"), Some(&Cmd::FocusDown));
    }

    #[test]
    fn prefix_parse_accepts_safe_function_keys_and_modified_characters() {
        assert_eq!(PrefixSpec::parse("ctrl+space"), Some(PrefixSpec::default()));
        assert_eq!(PrefixSpec::parse("control+b").unwrap().spec(), "ctrl+b");
        assert_eq!(PrefixSpec::parse("f12").unwrap().label(), "F12");
        assert_eq!(PrefixSpec::parse("shift+f12").unwrap().label(), "Shift+F12");
        assert_eq!(PrefixSpec::parse("option+\\").unwrap().spec(), "alt+\\");
        assert_eq!(PrefixSpec::parse("ctrl+plus").unwrap().label(), "Ctrl++");
        // Bare text and Shift-only text are rejected so they cannot swallow typing.
        assert_eq!(PrefixSpec::parse("b"), None);
        assert_eq!(PrefixSpec::parse("space"), None);
        assert_eq!(PrefixSpec::parse("shift+b"), None);
        assert_eq!(PrefixSpec::parse("f13"), None);
        assert_eq!(PrefixSpec::parse("nonsense"), None);
    }

    #[test]
    fn prefix_matches_the_exact_configured_chord() {
        let ctrl = KeyModifiers::CONTROL;
        // Ctrl+Space accepts all three terminal encodings.
        let sp = PrefixSpec::default();
        assert!(sp.matches(&KeyEvent::new(KeyCode::Char(' '), ctrl)));
        assert!(sp.matches(&KeyEvent::new(KeyCode::Char('@'), ctrl)));
        assert!(sp.matches(&KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)));
        assert!(!sp.matches(&KeyEvent::new(KeyCode::Char(' '), ctrl | KeyModifiers::ALT)));
        assert!(!sp.matches(&KeyEvent::new(KeyCode::Char('b'), ctrl)));

        let cb = PrefixSpec::parse("ctrl+b").unwrap();
        assert!(cb.matches(&KeyEvent::new(KeyCode::Char('b'), ctrl)));
        assert!(!cb.matches(&KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)));
        assert!(!cb.matches(&KeyEvent::new(
            KeyCode::Char('b'),
            ctrl | KeyModifiers::SHIFT
        )));

        assert_eq!(PrefixSpec::parse("ctrl+shift+b"), None);

        let shifted = PrefixSpec::parse("shift+f12").unwrap();
        assert!(shifted.matches(&KeyEvent::new(KeyCode::F(12), KeyModifiers::SHIFT)));
        assert!(!shifted.matches(&KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)));

        let f12 = PrefixSpec::parse("f12").unwrap();
        assert!(f12.matches(&KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)));
        assert!(!f12.matches(&KeyEvent::new(KeyCode::F(12), KeyModifiers::SHIFT)));
        assert_eq!(
            f12.key_event(),
            KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)
        );
    }

    #[test]
    fn apply_tmux_preset_sets_prefix_and_split_keys() {
        let _env = crate::persist::test_env("tmux-preset");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        assert!(app.apply_preset("function"));
        assert_eq!(app.prefix, PrefixSpec::parse("f12").unwrap());
        assert_eq!(app.current_preset(), Some(1));

        assert!(app.apply_preset("tmux"));
        assert_eq!(app.prefix, PrefixSpec::parse("ctrl+b").unwrap());
        assert_eq!(app.keymap.get("%"), Some(&Cmd::SplitRight));
        assert_eq!(app.keymap.get("\""), Some(&Cmd::SplitDown));
        // tmux pane/window/session keys map onto luvus's commands.
        assert_eq!(app.keymap.get("o"), Some(&Cmd::NextPane));
        assert_eq!(app.keymap.get(","), Some(&Cmd::RenameTab));
        assert_eq!(app.keymap.get(")"), Some(&Cmd::NextWorkspace));
        assert_eq!(app.keymap.get("("), Some(&Cmd::PrevWorkspace));
        // The default split keys are gone under the preset.
        assert_ne!(app.keymap.get("v"), Some(&Cmd::SplitRight));
        // `default` restores luvus's own prefix and keys.
        assert!(app.apply_preset("default"));
        assert_eq!(app.prefix, PrefixSpec::default());
        assert_eq!(app.keymap.get("v"), Some(&Cmd::SplitRight));
        assert!(!app.apply_preset("bogus"));
    }

    #[test]
    fn set_prefix_accepts_function_keys_but_rejects_plain_text() {
        let _env = crate::persist::test_env("set-prefix");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        assert!(app.set_prefix("f12"));
        assert_eq!(app.prefix, PrefixSpec::parse("f12").unwrap());
        assert_eq!(app.config.prefix, "f12");
        // A plain text spec is refused and the previous prefix is kept.
        assert!(!app.set_prefix("x"));
        assert_eq!(app.prefix, PrefixSpec::parse("f12").unwrap());
    }

    #[test]
    fn prefix_question_opens_help_and_any_key_closes() {
        use crate::event::AppEvent;
        use ratatui::crossterm::event::KeyModifiers;
        let prefix = || AppEvent::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let ch = |c| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        assert!(!app.help_open);
        app.handle_event(prefix());
        app.handle_event(ch('?')); // Ctrl+Space ? opens the cheat-sheet
        assert!(app.help_open, "? opened the help overlay");
        app.handle_event(ch('x')); // any key dismisses it (and is swallowed)
        assert!(!app.help_open, "next key closed the overlay");
        // The swallowed key must not have acted (e.g. closed a pane).
        assert_eq!(app.panes.len(), 1);
    }

    #[test]
    fn command_works_as_both_two_step_and_held_chord() {
        let _env = crate::persist::test_env("two-step-chord");
        use crate::event::AppEvent;
        use ratatui::crossterm::event::KeyModifiers;
        let prefix = || AppEvent::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        let key = |c, m| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), m));

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let tabs = app.ws().tabs.len();

        // Two-step: Ctrl+Space, release, then plain `c`.
        app.handle_event(prefix());
        app.handle_event(key('c', KeyModifiers::NONE));
        assert_eq!(app.ws().tabs.len(), tabs + 1, "two-step prefix opens a tab");

        // Held chord: Ctrl+Space then Ctrl+c (Ctrl never released).
        app.handle_event(prefix());
        app.handle_event(key('c', KeyModifiers::CONTROL));
        assert_eq!(app.ws().tabs.len(), tabs + 2, "held chord opens a tab too");

        // The same held-chord works for `v` (split): Ctrl+Space+Ctrl+v.
        let panes = app.layout().len();
        app.handle_event(prefix());
        app.handle_event(key('v', KeyModifiers::CONTROL));
        assert_eq!(
            app.layout().len(),
            panes + 1,
            "Ctrl+Space+v splits the pane"
        );
    }

    #[test]
    fn status_bar_shows_search_and_the_live_prefix() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("status-prefix-hint");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(120, 24, tx).unwrap();

        let screen = |app: &mut App| -> String {
            let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
            term.draw(|f| crate::ui::render(f, app)).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<Vec<_>>()
                .join("")
        };

        // Normal mode shows the live prefix chord (default Ctrl+Space).
        assert!(
            screen(&mut app).contains("Ctrl+Space"),
            "default prefix shown"
        );

        // Enter prefix mode: the hint bar includes the search key `/`.
        app.handle_event(crate::event::AppEvent::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::CONTROL,
        )));
        let bar = screen(&mut app);
        assert!(
            bar.contains('/'),
            "prefix hint bar shows the `/` search key"
        );

        // Back in normal mode, after switching to Ctrl+b the readout reflects it.
        app.handle_event(crate::event::AppEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        assert!(app.set_prefix("ctrl+b"));
        assert!(
            screen(&mut app).contains("Ctrl+B"),
            "status bar reflects the custom prefix"
        );
    }

    #[test]
    fn a_custom_prefix_drives_command_mode() {
        let _env = crate::persist::test_env("custom-prefix");
        use crate::event::AppEvent;
        let key = |c, m| AppEvent::Key(KeyEvent::new(KeyCode::Char(c), m));

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        // Switch the prefix to Ctrl+b (the tmux chord).
        assert!(app.set_prefix("ctrl+b"));
        let tabs = app.ws().tabs.len();

        // Ctrl+b then `c` opens a tab through the configured prefix.
        app.handle_event(key('b', KeyModifiers::CONTROL));
        app.handle_event(key('c', KeyModifiers::NONE));
        assert_eq!(app.ws().tabs.len(), tabs + 1, "Ctrl+b prefix works");

        // The old Ctrl+Space is no longer the prefix: `c` after it does nothing.
        app.handle_event(key(' ', KeyModifiers::CONTROL));
        app.handle_event(key('c', KeyModifiers::NONE));
        assert_eq!(
            app.ws().tabs.len(),
            tabs + 1,
            "Ctrl+Space no longer opens command mode"
        );
    }

    #[test]
    fn function_key_prefix_drives_command_mode() {
        let _env = crate::persist::test_env("function-prefix");
        use crate::event::AppEvent;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        assert!(app.set_prefix("f12"));
        let tabs = app.ws().tabs.len();

        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::F(12),
            KeyModifiers::NONE,
        )));
        app.handle_event(AppEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.ws().tabs.len(), tabs + 1, "F12 prefix opens a tab");
    }

    #[test]
    fn next_pane_cycles_and_rename_tab_opens_the_modal() {
        let _env = crate::persist::test_env("next-pane-rename");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        // One pane: cycling is a no-op.
        let only = app.layout().focus;
        app.run_cmd(Cmd::NextPane);
        assert_eq!(app.layout().focus, only, "single pane: no cycle");

        // Split into two, then `o` cycles focus between them and wraps.
        app.run_cmd(Cmd::SplitRight);
        let leaves = app.layout().leaves();
        assert_eq!(leaves.len(), 2);
        let start = app.layout().focus;
        app.run_cmd(Cmd::NextPane);
        let second = app.layout().focus;
        assert_ne!(second, start, "moved to the other pane");
        app.run_cmd(Cmd::NextPane);
        assert_eq!(app.layout().focus, start, "wrapped back");

        // RenameTab opens the tab-rename modal for the active tab.
        assert!(app.tab_rename.is_none());
        app.run_cmd(Cmd::RenameTab);
        assert!(app.tab_rename.is_some(), "rename tab opened the modal");
    }

    #[test]
    fn next_attention_cycles_blocked_agents() {
        let _env = crate::persist::test_env("next-attention");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();

        // Two panes in the tab, both marked as blocked agents.
        app.run_cmd(Cmd::SplitRight);
        let ids = app.layout().leaves();
        assert_eq!(ids.len(), 2, "split gave two panes");
        for &id in &ids {
            let mut st = PaneStatus::new("claude".to_string());
            st.state = crate::ui::theme::State::Blocked;
            app.status.insert(id, st);
        }

        // From whichever pane is focused, cycling reaches the other, then wraps.
        let start = app.layout().focus;
        app.focus_next_attention();
        let second = app.layout().focus;
        assert_ne!(second, start, "moved to the other waiting agent");
        app.focus_next_attention();
        assert_eq!(app.layout().focus, start, "wrapped back to the first");
    }

    #[test]
    fn next_attention_no_blocked_keeps_focus() {
        let _env = crate::persist::test_env("next-attention-none");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        app.run_cmd(Cmd::SplitRight);
        let before = app.layout().focus;
        // No pane is in the Blocked state → focus must not move.
        app.focus_next_attention();
        assert_eq!(
            app.layout().focus,
            before,
            "no waiting agents: focus unchanged"
        );
    }
}
