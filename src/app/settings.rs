//! The Settings modal — transient UI state plus open/close, key & click
//! handling, and the per-tab apply logic that mutates `App.config`, applies the
//! change live, and persists it. See docs/15.

use super::*;
use crate::config;
use crate::ui::theme;

/// The Keys tab leads with two special selectable rows before the command list
/// (docs/64): the prefix chord, then the preset chooser.
pub const KEYS_PREFIX_ROW: usize = 0;
pub const KEYS_PRESET_ROW: usize = 1;
pub const KEYS_HEADER_ROWS: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsTab {
    General,
    Theme,
    Layout,
    Keys,
    Modules,
    Integrations,
    Language,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 7] = [
        SettingsTab::General,
        SettingsTab::Theme,
        SettingsTab::Layout,
        SettingsTab::Keys,
        SettingsTab::Modules,
        SettingsTab::Integrations,
        SettingsTab::Language,
    ];

    pub fn icon(self) -> &'static str {
        match self {
            SettingsTab::General => "◆",
            SettingsTab::Theme => "◑",
            SettingsTab::Layout => "▣",
            SettingsTab::Keys => "⌨",
            SettingsTab::Modules => "❏",
            SettingsTab::Integrations => "☆",
            SettingsTab::Language => "⊕",
        }
    }

    /// The tab label in the active UI language (docs/21).
    pub fn label(self, cat: &crate::i18n::Catalog) -> &'static str {
        match self {
            SettingsTab::General => cat.tab_general,
            SettingsTab::Theme => cat.tab_theme,
            SettingsTab::Layout => cat.tab_layout,
            SettingsTab::Keys => cat.tab_keys,
            SettingsTab::Modules => cat.tab_modules,
            SettingsTab::Integrations => cat.tab_agents,
            SettingsTab::Language => cat.tab_language,
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn from_index(i: usize) -> SettingsTab {
        Self::ALL[i % Self::ALL.len()]
    }
}

/// Transient state of the open Settings modal.
pub struct SettingsUi {
    pub tab: SettingsTab,
    pub cursor: usize,
    /// Candidate prefix captured once and waiting for the same chord again.
    /// Nothing is persisted until confirmation succeeds.
    pub prefix_candidate: Option<String>,
    /// First visual row shown in the Layout tab. Persisting this while the modal
    /// is open prevents a visible dock/bar button click from re-anchoring the
    /// list around its newly selected row.
    pub layout_scroll: usize,
    /// In the Keys tab: capturing the next key press to rebind `cursor`'s command.
    pub capturing: bool,
}

/// A selectable row in the Layout tab (docs/15 + docs/29). The pane-layout and
/// DIFF rows come first, then a `── Docks ──` divider with sidebar and dock
/// controls. `Dock` rows carry `[Left] [Right]` place buttons.
#[derive(Clone)]
pub enum LayoutRow {
    SidebarWidth,
    ColGap,
    RowGap,
    Scrollback,
    MobileWidth,
    PaneTitles,
    PaneTitlePath,
    ResumeWs,
    DiffLayout,
    DiffWrap,
    DiffContext,
    DiffLineNumbers,
    DiffMarkers,
    DiffColors,
    DiffLiveRefresh,
    #[cfg(windows)]
    Shell,
    LeftVisible,
    RightVisible,
    RightWidth,
    Dock(DockKind),
    Bar(String),
}

/// A selectable row in the General tab: the app-wide preferences that are not
/// about looks or layout. The file-open control comes first, then a
/// `── Notifications ──` section (same blank-gap + divider treatment as the
/// Layout tab's Docks section).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GeneralRow {
    FileOpen,
    FilesShowHidden,
    ShiftEnter,
    CheckUpdates,
    /// Replay each agent's own CLI options on resume (docs/62).
    ResumeFlags,
    /// Open a new tab/split at the workspace root instead of inheriting the
    /// focused pane's cwd.
    NewPaneToWorkspaceRoot,
    /// Show each agent's live session title in the AGENTS sidebar.
    AgentTitle,
    SoundDone,
    SoundBlocked,
    TestSound,
}

/// A selectable row in the Modules tab (docs/13 §3.6): a module, or one of the
/// settings it declares (indented beneath it while the module is enabled).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModuleRow {
    Module(usize),
    Setting(usize, usize),
}

/// Cap a module setting's typed value, so a pathological paste can't bloat the
/// module's `settings.json`.
const MODULE_SETTING_MAX: usize = 512;

impl App {
    /// The General tab's ordered selectable rows.
    pub fn general_rows(&self) -> Vec<GeneralRow> {
        vec![
            GeneralRow::FileOpen,
            GeneralRow::FilesShowHidden,
            GeneralRow::ShiftEnter,
            GeneralRow::CheckUpdates,
            GeneralRow::ResumeFlags,
            GeneralRow::NewPaneToWorkspaceRoot,
            GeneralRow::AgentTitle,
            GeneralRow::SoundDone,
            GeneralRow::SoundBlocked,
            GeneralRow::TestSound,
        ]
    }

    /// Index of the first notification row (where the `── Notify ──` divider
    /// goes), mirroring `dock_section_start` in the Layout tab. The six general
    /// settings sit above it.
    pub fn general_section_start(&self) -> usize {
        6
    }

    /// The Layout tab's ordered selectable rows (docs/29). The first index of the
    /// dock section (used to draw the `── Docks ──` divider) is `dock_section_start`.
    pub fn layout_rows(&self) -> Vec<LayoutRow> {
        let mut v = vec![
            LayoutRow::ColGap,
            LayoutRow::RowGap,
            LayoutRow::Scrollback,
            LayoutRow::MobileWidth,
            LayoutRow::PaneTitles,
            LayoutRow::PaneTitlePath,
            LayoutRow::ResumeWs,
        ];
        #[cfg(windows)]
        v.push(LayoutRow::Shell);
        v.push(LayoutRow::DiffLayout);
        v.push(LayoutRow::DiffWrap);
        v.push(LayoutRow::DiffContext);
        v.push(LayoutRow::DiffLineNumbers);
        v.push(LayoutRow::DiffMarkers);
        v.push(LayoutRow::DiffColors);
        v.push(LayoutRow::DiffLiveRefresh);
        v.push(LayoutRow::LeftVisible);
        v.push(LayoutRow::RightVisible);
        v.push(LayoutRow::SidebarWidth);
        v.push(LayoutRow::RightWidth);
        for k in self.available_docks() {
            v.push(LayoutRow::Dock(k));
        }
        for key in self.bar_setting_keys() {
            v.push(LayoutRow::Bar(key));
        }
        v
    }

    fn bar_setting_keys(&self) -> Vec<String> {
        // Settings rows describe declarations, not their placement. Keeping
        // this order stable prevents a Top/Bottom click from moving the row
        // under the pointer while the user is arranging widgets.
        self.bar.declarations.keys().cloned().collect()
    }

    pub fn bar_section_start(&self) -> usize {
        self.layout_rows()
            .iter()
            .position(|row| matches!(row, LayoutRow::Bar(_)))
            .unwrap_or(usize::MAX)
    }

    pub fn diff_section_start(&self) -> usize {
        self.layout_rows()
            .iter()
            .position(|row| matches!(row, LayoutRow::DiffLayout))
            .unwrap_or(usize::MAX)
    }

    /// Index of the first dock-section row (where the `── Docks ──` divider goes).
    pub fn dock_section_start(&self) -> usize {
        // Keep in step with `layout_rows`: the pane-layout and DIFF rows before
        // the sidebar controls and dock-placement rows.
        self.layout_rows()
            .iter()
            .position(|row| matches!(row, LayoutRow::LeftVisible))
            .unwrap_or(usize::MAX)
    }

    /// Open Settings on the **first** tab (General). Switching to Theme still
    /// preselects the active palette, via `settings_set_tab`.
    pub fn open_settings(&mut self) {
        self.settings = Some(SettingsUi {
            tab: SettingsTab::General,
            cursor: 0,
            prefix_candidate: None,
            layout_scroll: 0,
            capturing: false,
        });
    }

    pub fn close_settings(&mut self) {
        self.settings = None;
        self.module_setting_edit = None;
    }

    /// Open the changelog modal (click the status-line version number), scrolled to
    /// the top so the newest release is shown first.
    pub fn open_changelog(&mut self) {
        self.changelog_open = true;
        self.changelog_scroll = 0;
        self.changelog_rows = None; // rebuilt on the next draw
                                    // Ask again while the window is open. Opening the changelog *is* the
                                    // question "am I current?", and the periodic check may not have come
                                    // round since the release landed.
        if self.config.check_updates {
            crate::update::check_now(self.app_tx.clone());
        }
    }

    /// Number of selectable control rows in `tab` (for cursor clamping + render).
    pub fn settings_rows(&self, tab: SettingsTab) -> usize {
        match tab {
            SettingsTab::General => self.general_rows().len(),
            SettingsTab::Theme => self.theme_registry.entries().len(),
            SettingsTab::Layout => self.layout_rows().len(),
            // Rebindable commands first, then the read-only reference rows — the
            // cursor steps through both so the whole reference is keyboard-reachable.
            SettingsTab::Keys => {
                KEYS_HEADER_ROWS + crate::app::Cmd::ALL.len() + crate::app::key_reference_rows()
            }
            SettingsTab::Modules => self.module_rows().len(),
            SettingsTab::Integrations => crate::integration::AGENTS.len(),
            SettingsTab::Language => crate::i18n::LANGS.len(),
        }
    }

    pub fn handle_settings_key(&mut self, key: KeyEvent) {
        let Some((tab, cursor, capturing, prefix_candidate)) = self
            .settings
            .as_ref()
            .map(|ui| (ui.tab, ui.cursor, ui.capturing, ui.prefix_candidate.clone()))
        else {
            return;
        };
        // Keys tab: while capturing, the next key press *is* the new binding
        // (Esc cancels). This must intercept before the normal handling so keys
        // like Tab / digits can themselves be bound.
        if capturing {
            if cursor == KEYS_PREFIX_ROW {
                if key.code == KeyCode::Esc {
                    if let Some(ui) = self.settings.as_mut() {
                        ui.capturing = false;
                        ui.prefix_candidate = None;
                    }
                    return;
                }
                let Some(spec) = Self::prefix_spec_from_key(&key) else {
                    self.show_toast("Use F1-F12 or a Ctrl/Alt chord");
                    return;
                };
                if prefix_candidate.as_deref() == Some(spec.as_str()) {
                    self.set_prefix(&spec);
                    if let Some(ui) = self.settings.as_mut() {
                        ui.capturing = false;
                        ui.prefix_candidate = None;
                    }
                } else {
                    let label = keys::PrefixSpec::parse(&spec)
                        .map(|prefix| prefix.label())
                        .unwrap_or(spec.clone());
                    if let Some(ui) = self.settings.as_mut() {
                        ui.prefix_candidate = Some(spec);
                    }
                    self.show_toast(format!("Press {label} again to confirm"));
                }
                return;
            }
            if key.code != KeyCode::Esc {
                if let (Some(cmd), Some(s)) = (Self::keys_cmd_at(cursor), keys::key_string(&key)) {
                    self.rebind(cmd, s);
                }
            }
            if let Some(ui) = self.settings.as_mut() {
                ui.capturing = false;
                ui.prefix_candidate = None;
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.close_settings(),
            KeyCode::Tab => self.settings_set_tab(SettingsTab::from_index(tab.index() + 1)),
            KeyCode::BackTab => self.settings_set_tab(SettingsTab::from_index(
                tab.index() + SettingsTab::ALL.len() - 1,
            )),
            KeyCode::Up => self.settings_move(-1),
            KeyCode::Down => self.settings_move(1),
            KeyCode::Left => self.settings_adjust(cursor, -1),
            KeyCode::Right => self.settings_adjust(cursor, 1),
            KeyCode::Enter if tab == SettingsTab::Theme => self.settings_enter_theme(cursor),
            KeyCode::Enter | KeyCode::Char(' ') => self.settings_activate(cursor),
            // In the Keys tab, Backspace/Delete resets a binding to its default:
            // the prefix row back to Ctrl+Space, a command row to its default key
            // (a no-op on the preset / reference rows, which have nothing to reset).
            KeyCode::Backspace | KeyCode::Delete if tab == SettingsTab::Keys => {
                if cursor == KEYS_PREFIX_ROW {
                    self.set_prefix("ctrl+space");
                } else if let Some(cmd) = Self::keys_cmd_at(cursor) {
                    self.reset_binding(cmd);
                }
            }
            KeyCode::Char(c) if ('1'..='7').contains(&c) => {
                self.settings_set_tab(SettingsTab::from_index(c as usize - '1' as usize));
            }
            _ => {}
        }
    }

    /// Route a click while the modal is open (close / switch tab / hit a control).
    pub fn handle_settings_click(&mut self, c: u16, r: u16) {
        let hit = |rect: Rect| c >= rect.x && c < rect.right() && r >= rect.y && r < rect.bottom();
        if self.settings_close_rect.is_some_and(hit) {
            self.close_settings();
            return;
        }
        // A click outside the modal dismisses it.
        if self.settings_modal_rect.is_some_and(|m| !hit(m)) {
            self.close_settings();
            return;
        }
        if let Some((tab, _)) = self
            .settings_tab_rects
            .iter()
            .find(|(_, rect)| hit(*rect))
            .copied()
        {
            self.settings_set_tab(tab);
            return;
        }
        // Installed themes expose a separate right-aligned remove action. Handle
        // it before the row body so removal never previews/selects the target.
        if self
            .settings
            .as_ref()
            .is_some_and(|ui| ui.tab == SettingsTab::Theme)
        {
            if let Some(id) = self
                .settings_theme_remove_rects
                .iter()
                .find(|(_, rect)| hit(*rect))
                .map(|(id, _)| id.clone())
            {
                self.request_theme_uninstall(&id);
                return;
            }
        }
        // A click on a slider arrow steps that control in its direction.
        if let Some((i, delta, _)) = self
            .settings_arrow_rects
            .iter()
            .find(|(_, _, rect)| hit(*rect))
            .copied()
        {
            if let Some(ui) = self.settings.as_mut() {
                ui.cursor = i;
            }
            self.settings_adjust(i, delta);
            return;
        }
        // A click on a control row selects it, and activates it unless it's a
        // slider (those only change via their ‹ › arrows).
        if let Some((i, _)) = self
            .settings_ctl_rects
            .iter()
            .find(|(_, rect)| hit(*rect))
            .map(|(i, rect)| (*i, *rect))
        {
            let tab = self.settings.as_ref().map(|u| u.tab);
            if let Some(ui) = self.settings.as_mut() {
                ui.cursor = i;
            }
            // Slider/button rows only change via their arrows/buttons, so a click
            // on the row body just selects it: the Layout width sliders and dock
            // `[Left] [Right]` place rows.
            let is_slider = match tab {
                Some(SettingsTab::Layout) => matches!(
                    self.layout_rows().get(i),
                    Some(LayoutRow::SidebarWidth)
                        | Some(LayoutRow::RightWidth)
                        | Some(LayoutRow::MobileWidth)
                        | Some(LayoutRow::DiffContext)
                        | Some(LayoutRow::Dock(_))
                        | Some(LayoutRow::Bar(_))
                ),
                // The file-open chooser only moves via its `‹ ›` arrows.
                Some(SettingsTab::General) => {
                    self.general_rows().get(i) == Some(&GeneralRow::FileOpen)
                }
                // Number/enum module settings likewise only move via `‹ ›`.
                Some(SettingsTab::Modules) => self.module_row_is_slider(i),
                _ => false,
            };
            if !is_slider {
                self.settings_activate(i);
            }
        }
    }

    fn settings_set_tab(&mut self, tab: SettingsTab) {
        let cursor = match tab {
            SettingsTab::Theme => self
                .theme_registry
                .index_of(&self.config.theme)
                .unwrap_or(0),
            SettingsTab::Language => lang_cursor(&self.config.language),
            _ => 0,
        };
        if let Some(ui) = self.settings.as_mut() {
            ui.tab = tab;
            ui.cursor = cursor;
            ui.prefix_candidate = None;
            ui.layout_scroll = 0;
            ui.capturing = false;
        }
    }

    /// Mouse-wheel scroll in the open modal: nudge the selection a few rows so a
    /// long list (the Keys reference, the theme list) scrolls without holding the
    /// arrows. `dir` is -1 (up) or +1 (down).
    pub fn settings_scroll(&mut self, dir: i32) {
        self.settings_move(dir * 3);
    }

    fn settings_move(&mut self, delta: i32) {
        let Some(&SettingsUi { tab, cursor, .. }) = self.settings.as_ref() else {
            return;
        };
        let rows = self.settings_rows(tab);
        if rows == 0 {
            return;
        }
        let new = (cursor as i32 + delta).clamp(0, rows as i32 - 1) as usize;
        if let Some(ui) = self.settings.as_mut() {
            ui.cursor = new;
        }
        // Theme / Language preview live as the selection moves.
        if tab == SettingsTab::Theme {
            if let Some(id) = self
                .theme_registry
                .entries()
                .get(new)
                .map(|entry| entry.id.clone())
            {
                self.apply_theme(&id);
            }
        } else if tab == SettingsTab::Language {
            self.apply_language(crate::i18n::LANGS[new]);
        }
    }

    fn settings_adjust(&mut self, cursor: usize, delta: i32) {
        let Some(tab) = self.settings.as_ref().map(|u| u.tab) else {
            return;
        };
        match tab {
            // radio tabs: ‹ › move the selection like up/down
            SettingsTab::Theme | SettingsTab::Language => self.settings_move(delta),
            SettingsTab::Layout => self.adjust_layout(cursor, delta),
            SettingsTab::General => self.adjust_general(cursor, delta),
            // Only the preset row responds to ‹ › (it cycles presets); rebinding a
            // command is Enter (capture), and the prefix row captures a chord.
            SettingsTab::Keys if cursor == KEYS_PRESET_ROW => self.cycle_preset(delta),
            SettingsTab::Keys => {}
            SettingsTab::Integrations => self.settings_activate(cursor),
            SettingsTab::Modules => self.toggle_module(cursor, Some(delta)),
        }
    }

    fn settings_activate(&mut self, cursor: usize) {
        let Some(tab) = self.settings.as_ref().map(|u| u.tab) else {
            return;
        };
        if let Some(ui) = self.settings.as_mut() {
            ui.capturing = false;
            ui.prefix_candidate = None;
        }
        match tab {
            SettingsTab::Theme => {
                let index = cursor.min(self.theme_registry.entries().len().saturating_sub(1));
                if let Some(id) = self
                    .theme_registry
                    .entries()
                    .get(index)
                    .map(|entry| entry.id.clone())
                {
                    self.apply_theme(&id);
                }
            }
            SettingsTab::Language => {
                self.apply_language(crate::i18n::LANGS[cursor.min(crate::i18n::LANGS.len() - 1)])
            }
            SettingsTab::Layout => self.activate_layout(cursor),
            // Enter/click: the Test row rings the chime, everything else steps.
            SettingsTab::General => match self.general_rows().get(cursor).copied() {
                Some(GeneralRow::TestSound) => self.test_sound(),
                _ => self.adjust_general(cursor, 1),
            },
            // Enter on a rebindable Keys row starts capturing the next key as its
            // binding; on a reference row there's nothing to capture.
            SettingsTab::Keys => match cursor {
                // The preset row applies the next preset on Enter/click.
                KEYS_PRESET_ROW => self.cycle_preset(1),
                // The prefix row and every command row capture the next key.
                KEYS_PREFIX_ROW => {
                    if let Some(ui) = self.settings.as_mut() {
                        ui.capturing = true;
                        ui.prefix_candidate = None;
                    }
                }
                _ => {
                    if Self::keys_cmd_at(cursor).is_some() {
                        if let Some(ui) = self.settings.as_mut() {
                            ui.capturing = true;
                            ui.prefix_candidate = None;
                        }
                    }
                }
            },
            SettingsTab::Integrations => self.install_integration(cursor),
            SettingsTab::Modules => self.toggle_module(cursor, None),
        }
    }

    /// The command at row `cursor` in the Keys tab, or `None` when the cursor is
    /// on a read-only reference row (which lives past the command list).
    fn keys_cmd_at(cursor: usize) -> Option<crate::app::Cmd> {
        cursor
            .checked_sub(KEYS_HEADER_ROWS)
            .and_then(|i| crate::app::Cmd::ALL.get(i).copied())
    }

    /// Convert a captured event to one canonical safe prefix. Function keys can
    /// stand alone; character and Space prefixes require Ctrl or Alt. Super,
    /// Hyper, and Meta are rejected because desktop environments commonly keep
    /// them before the terminal can report them consistently.
    fn prefix_spec_from_key(key: &KeyEvent) -> Option<String> {
        if key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
        {
            return None;
        }
        if key.code == KeyCode::Null {
            return Some("ctrl+space".to_string());
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let mut shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let base = match key.code {
            KeyCode::Char('@') if ctrl => {
                // Ctrl+Space may arrive as the physical Ctrl+Shift+2 (`@`). The
                // resulting terminal key is still NUL, so persist Ctrl+Space.
                shift = false;
                "space".to_string()
            }
            KeyCode::Char(' ') if ctrl => "space".to_string(),
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char('+') => "plus".to_string(),
            KeyCode::Char(character) if character.is_ascii() => {
                character.to_ascii_lowercase().to_string()
            }
            KeyCode::F(number @ 1..=12) => format!("f{number}"),
            _ => return None,
        };
        let mut parts = Vec::new();
        if ctrl {
            parts.push("ctrl");
        }
        if alt {
            parts.push("alt");
        }
        if shift {
            parts.push("shift");
        }
        parts.push(&base);
        let candidate = parts.join("+");
        keys::PrefixSpec::parse(&candidate).map(|prefix| prefix.spec())
    }

    /// The index of the preset that exactly matches the current config, or `None`
    /// when the user has customized beyond any preset ("Custom").
    pub fn current_preset(&self) -> Option<usize> {
        keys::presets().iter().position(|p| {
            p.prefix == self.config.prefix
                && self.config.keybindings.len() == p.binds.len()
                && p.binds
                    .iter()
                    .all(|(id, k)| self.config.keybindings.get(*id).map(|v| v == k) == Some(true))
        })
    }

    /// Cycle to the next/previous preset and apply it (the Preset row's `‹ ›`).
    fn cycle_preset(&mut self, delta: i32) {
        let ps = keys::presets();
        let n = ps.len() as i32;
        if n == 0 {
            return;
        }
        let cur = self.current_preset().unwrap_or(0) as i32;
        let next = (((cur + delta) % n) + n) % n;
        self.apply_preset(ps[next as usize].id);
    }

    /// The Modules tab's dynamic row model: one row per installed module,
    /// followed by an indented row per setting it declares while it is enabled.
    /// Disabled modules collapse, so the list stays short.
    pub fn module_rows(&self) -> Vec<ModuleRow> {
        let mut v = Vec::new();
        for (mi, m) in self.modules.modules.iter().enumerate() {
            v.push(ModuleRow::Module(mi));
            if m.enabled && m.warning.is_none() {
                v.extend((0..m.manifest.settings.len()).map(|si| ModuleRow::Setting(mi, si)));
            }
        }
        v
    }

    /// Whether Modules row `i` is a `‹ ›` stepper (number/enum), which a click on
    /// the row body should only select, not change.
    fn module_row_is_slider(&self, i: usize) -> bool {
        use crate::module::manifest::SettingKind;
        let Some(ModuleRow::Setting(mi, si)) = self.module_rows().get(i).copied() else {
            return false;
        };
        self.modules
            .modules
            .get(mi)
            .and_then(|m| m.manifest.settings.get(si))
            .is_some_and(|s| matches!(s.kind, SettingKind::Number | SettingKind::Enum))
    }

    /// Enable/disable the module at `cursor`, or step its setting. `delta` is
    /// the direction for a `‹ ›` press; `None` means "activate" (Enter/click),
    /// which toggles a bool and opens the prompt for a string.
    fn toggle_module(&mut self, cursor: usize, delta: Option<i32>) {
        match self.module_rows().get(cursor).copied() {
            Some(ModuleRow::Module(mi)) => {
                if let Some(m) = self.modules.modules.get(mi) {
                    let (id, on) = (m.id.clone(), !m.enabled);
                    let _ = self.module_set_enabled(&id, on);
                    // Collapsing a module can leave the cursor past the end.
                    self.clamp_settings_cursor();
                }
            }
            Some(ModuleRow::Setting(mi, si)) => self.adjust_module_setting(mi, si, delta),
            None => {}
        }
    }

    /// Apply a step (or an activation) to one declared module setting.
    fn adjust_module_setting(&mut self, mi: usize, si: usize, delta: Option<i32>) {
        use crate::module::manifest::SettingKind;
        let Some((id, spec)) = self.modules.modules.get(mi).and_then(|m| {
            m.manifest
                .settings
                .get(si)
                .map(|s| (m.id.clone(), s.clone()))
        }) else {
            return;
        };
        let current = crate::module::settings::get(
            &self.modules.find(&id).unwrap().manifest.clone(),
            &id,
            &spec.key,
        )
        .unwrap_or_else(|| spec.default_value());

        // A string setting has nothing to step — Enter (and either arrow) opens
        // the inline prompt instead.
        if spec.kind == SettingKind::String {
            self.module_setting_edit = Some(ModuleSettingEdit {
                module_id: id,
                key: spec.key.clone(),
                title: spec.title.clone(),
                // A secret starts empty rather than revealing the stored value.
                buffer: if spec.secret {
                    String::new()
                } else {
                    current.as_str().unwrap_or_default().to_string()
                },
                secret: spec.secret,
            });
            return;
        }
        // Enter on a bool flips it; on a number/enum it advances one step.
        let step = delta.unwrap_or(1) as i64;
        let next = crate::module::settings::stepped(&spec, &current, step);
        if let Err(e) = self.module_set_setting(&id, &spec.key, next) {
            self.show_toast(e);
        }
    }

    /// Key handling for the inline module-setting prompt (docs/13 §3.6).
    /// `Enter` saves, `Esc` cancels — the same contract as the rename modals.
    pub fn handle_module_setting_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.module_setting_edit = None,
            KeyCode::Enter => {
                if let Some(e) = self.module_setting_edit.take() {
                    let v = Value::String(e.buffer);
                    if let Err(err) = self.module_set_setting(&e.module_id, &e.key, v) {
                        self.show_toast(err);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(e) = self.module_setting_edit.as_mut() {
                    e.buffer.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(e) = self.module_setting_edit.as_mut() {
                    if e.buffer.chars().count() < MODULE_SETTING_MAX {
                        e.buffer.push(c);
                    }
                }
            }
            _ => {}
        }
    }

    /// Keep the settings cursor inside the current tab's row count (rows can
    /// shrink under it when a module collapses).
    fn clamp_settings_cursor(&mut self) {
        let Some(tab) = self.settings.as_ref().map(|u| u.tab) else {
            return;
        };
        let max = self.settings_rows(tab).saturating_sub(1);
        if let Some(ui) = self.settings.as_mut() {
            ui.cursor = ui.cursor.min(max);
        }
    }

    fn settings_enter_theme(&mut self, cursor: usize) {
        let index = cursor.min(self.theme_registry.entries().len().saturating_sub(1));
        if let Some((id, removable)) = self.theme_registry.entries().get(index).map(|entry| {
            (
                entry.id.clone(),
                matches!(
                    entry.source,
                    crate::theme::registry::ThemeSource::Local { .. }
                ),
            )
        }) {
            if removable {
                self.request_theme_uninstall(&id);
            } else {
                self.apply_theme(&id);
            }
        }
    }

    // ── apply helpers (mutate config, apply live, persist) ───────────────────

    /// Remove one installed theme from Settings without blocking the app loop on
    /// directory scans or filesystem writes. If it is active, first move to the
    /// bundled default so the install layer's active-theme guard remains intact.
    fn request_theme_uninstall(&mut self, id: &str) {
        if self.pending_theme_uninstalls.contains_key(id) {
            return;
        }
        let removable = self.theme_registry.get(id).is_some_and(|entry| {
            matches!(
                &entry.source,
                crate::theme::registry::ThemeSource::Local { .. }
            )
        });
        if !removable {
            self.show_toast(format!("Theme {id} is bundled and cannot be removed"));
            return;
        }

        let previous_theme = self.config.theme.clone();
        let restore = if theme::canonical(&previous_theme) == id {
            self.apply_theme(theme::THEMES[0]);
            Some((previous_theme, self.theme_selection_revision))
        } else {
            None
        };

        // Retire the rendered action immediately and record the in-flight worker
        // so subsequent repaints cannot recreate an actionable remove hitbox.
        self.settings_theme_remove_rects
            .retain(|(theme_id, _)| theme_id != id);
        self.pending_theme_uninstalls
            .insert(id.to_string(), restore);
        let tx = self.app_tx.clone();
        let id = id.to_string();
        self.show_toast(format!("Removing theme {id}…"));
        std::thread::spawn(move || {
            let result = crate::theme::install::uninstall(&id)
                .map(|_| crate::theme::ThemeRegistry::load())
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(crate::event::AppEvent::ThemeUninstalled { id, result });
        });
    }

    /// Apply the completed worker result on the single-writer app loop.
    pub(crate) fn finish_theme_uninstall(
        &mut self,
        id: String,
        result: Result<crate::theme::ThemeRegistry, String>,
    ) {
        let restore = self.pending_theme_uninstalls.remove(&id).flatten();
        match result {
            Ok(registry) => {
                self.replace_theme_registry(registry);
                self.settings_theme_remove_rects
                    .retain(|(theme_id, _)| theme_id != &id);
                self.clamp_settings_cursor();
                self.show_toast(format!("Removed theme {id}"));
            }
            Err(error) => {
                if let Some((previous_theme, fallback_revision)) = restore {
                    if self.theme_selection_revision == fallback_revision {
                        self.apply_theme(&previous_theme);
                    }
                }
                self.show_toast(format!("Could not remove {id}: {error}"));
            }
        }
    }

    pub(crate) fn theme_uninstall_pending(&self, id: &str) -> bool {
        self.pending_theme_uninstalls.contains_key(id)
    }

    pub(crate) fn apply_theme(&mut self, name: &str) {
        let Some(mut selected) = self.theme_registry.theme(name) else {
            return;
        };
        self.config.theme = theme::canonical(name).to_string();
        self.theme_selection_revision = self.theme_selection_revision.wrapping_add(1);
        if self.downsample {
            selected = selected.to_256();
        }
        self.theme = selected;
        self.changelog_rows = None;
        config::save(&self.config);
    }

    /// Swap the server's in-memory registry after an off-loop scan. A missing
    /// configured theme falls back visually without erasing its stored ID, so
    /// restoring the file and reloading brings the selection back.
    pub(crate) fn replace_theme_registry(&mut self, registry: crate::theme::ThemeRegistry) -> bool {
        let selected_exists = registry.get(&self.config.theme).is_some();
        if self.config.theme != "terminal" {
            let mut selected = registry.theme_or_default(&self.config.theme);
            if self.downsample {
                selected = selected.to_256();
            }
            self.theme = selected;
        }
        self.theme_registry = registry;
        self.clamp_settings_cursor();
        self.changelog_rows = None;
        selected_exists
    }

    /// Swap the UI language live + persist (docs/21) — mirrors `apply_theme`.
    fn apply_language(&mut self, code: &str) {
        self.config.language = code.to_string();
        self.catalog = crate::i18n::by_code(code);
        config::save(&self.config);
    }

    /// Layout tab ‹ ›/click on a row's control (docs/29). Width sliders step by
    /// `delta`; toggles flip; a `Dock` row's `[Left]`/`[Right]` buttons (which map
    /// to `delta < 0` / `delta > 0`) place the dock on that side.
    fn adjust_layout(&mut self, cursor: usize, delta: i32) {
        let Some(row) = self.layout_rows().get(cursor).cloned() else {
            return;
        };
        match row {
            LayoutRow::SidebarWidth => {
                let w = (self.sidebars.left.width as i32 + 2 * delta)
                    .clamp(SIDEBAR_WIDTH_MIN as i32, SIDEBAR_WIDTH_MAX as i32)
                    as u16;
                self.set_side_width(Side::Left, w);
            }
            LayoutRow::RightWidth => {
                let w = (self.sidebars.right.width as i32 + 2 * delta)
                    .clamp(SIDEBAR_WIDTH_MIN as i32, SIDEBAR_WIDTH_MAX as i32)
                    as u16;
                self.set_side_width(Side::Right, w);
            }
            LayoutRow::ColGap => {
                self.config.layout.col_gap ^= 1;
                self.apply_gaps();
            }
            LayoutRow::RowGap => {
                self.config.layout.row_gap ^= 1;
                self.apply_gaps();
            }
            LayoutRow::Scrollback => {
                let step = config::SCROLLBACK_BYTES_STEP as i64;
                let next = (self.config.scrollback_bytes() as i64 + step * delta as i64).clamp(
                    config::SCROLLBACK_BYTES_MIN as i64,
                    config::SCROLLBACK_BYTES_MAX as i64,
                ) as usize;
                self.config.layout.scrollback_bytes = Some(next);
                self.apply_history_budget();
                config::save(&self.config);
            }
            LayoutRow::MobileWidth => {
                let current = self.config.layout.mobile_width;
                self.config.layout.mobile_width = match (current, delta.cmp(&0)) {
                    (0, std::cmp::Ordering::Greater) => 24,
                    (0, _) => 0,
                    (24, std::cmp::Ordering::Less) => 0,
                    _ => (current as i32 + 4 * delta).clamp(24, 200) as u16,
                };
                config::save(&self.config);
            }
            LayoutRow::PaneTitles => {
                self.config.layout.show_titles = !self.config.layout.show_titles;
                config::save(&self.config);
            }
            LayoutRow::PaneTitlePath => {
                self.config.layout.pane_title_path = !self.config.layout.pane_title_path;
                config::save(&self.config);
            }
            LayoutRow::ResumeWs => {
                self.config.layout.resume_in_new_workspace =
                    !self.config.layout.resume_in_new_workspace;
                config::save(&self.config);
            }
            LayoutRow::DiffLayout => {
                self.config.layout.diff_layout = if delta < 0 {
                    match self.config.layout.diff_layout {
                        crate::diff::DiffLayoutPreference::Auto => {
                            crate::diff::DiffLayoutPreference::Stack
                        }
                        crate::diff::DiffLayoutPreference::Split => {
                            crate::diff::DiffLayoutPreference::Auto
                        }
                        crate::diff::DiffLayoutPreference::Stack => {
                            crate::diff::DiffLayoutPreference::Split
                        }
                    }
                } else {
                    self.config.layout.diff_layout.cycle()
                };
                config::save(&self.config);
            }
            LayoutRow::DiffWrap => {
                self.config.layout.diff_wrap = !self.config.layout.diff_wrap;
                config::save(&self.config);
            }
            LayoutRow::DiffContext => {
                self.config.layout.diff_context_lines =
                    (self.config.layout.diff_context_lines as i32 + delta)
                        .clamp(0, i32::from(crate::diff::MAX_CONTEXT_LINES))
                        as u16;
                config::save(&self.config);
            }
            LayoutRow::DiffLineNumbers => {
                self.config.layout.diff_show_line_numbers =
                    !self.config.layout.diff_show_line_numbers;
                config::save(&self.config);
            }
            LayoutRow::DiffMarkers => {
                self.config.layout.diff_marker_style = if delta < 0 {
                    self.config.layout.diff_marker_style.reverse()
                } else {
                    self.config.layout.diff_marker_style.cycle()
                };
                config::save(&self.config);
            }
            LayoutRow::DiffColors => {
                self.config.layout.diff_color_mode = self.config.layout.diff_color_mode.cycle();
                config::save(&self.config);
            }
            LayoutRow::DiffLiveRefresh => {
                self.config.layout.diff_live_refresh = !self.config.layout.diff_live_refresh;
                config::save(&self.config);
            }
            #[cfg(windows)]
            LayoutRow::Shell => self.cycle_shell(delta),
            LayoutRow::LeftVisible => {
                self.sidebars.left.visible = !self.sidebars.left.visible;
                self.save_sidebars();
            }
            LayoutRow::RightVisible => {
                self.sidebars.right.visible = !self.sidebars.right.visible;
                self.save_sidebars();
            }
            LayoutRow::Dock(kind) => {
                // Buttons encode the target as `delta`: -1 = Left, +1 = Right,
                // +2 = Off (unmount). `←`/`→` keys (∓1) place left/right.
                if delta <= -1 {
                    self.move_dock(&kind, Side::Left);
                } else if delta == 1 {
                    self.move_dock(&kind, Side::Right);
                } else {
                    self.unmount_dock(&kind);
                }
            }
            LayoutRow::Bar(key) => {
                let region = if delta < 0 {
                    Some(crate::bar::BarRegion::TopRight)
                } else if delta == 1 {
                    Some(crate::bar::BarRegion::BottomRight)
                } else {
                    None
                };
                self.config.bars.place(&key, region);
                self.bar.clear_geometry();
                config::save(&self.config);
                if let Some(next) = self
                    .layout_rows()
                    .iter()
                    .position(|row| matches!(row, LayoutRow::Bar(candidate) if candidate == &key))
                {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.cursor = next;
                    }
                }
            }
        }
    }

    /// Enter/click on a Layout row: bump a slider, flip a toggle, or (for a dock)
    /// cycle Left → Right → Off → Left.
    fn activate_layout(&mut self, cursor: usize) {
        match self.layout_rows().get(cursor).cloned() {
            // Cycle Left → Right → Off → Left, but **skip a full side** so the
            // cycle always makes progress (never stuck because the next side is at
            // the cap). Skipping is silent here — the explicit `[Left]/[Right]`
            // buttons still toast when you place directly onto a full side.
            Some(LayoutRow::Dock(kind)) => match self.sidebars.side_of(&kind) {
                Some(Side::Left) => {
                    if self.sidebars.has_room(Side::Right) {
                        self.move_dock(&kind, Side::Right);
                    } else {
                        self.unmount_dock(&kind);
                    }
                }
                Some(Side::Right) => self.unmount_dock(&kind),
                None => {
                    if self.sidebars.has_room(Side::Left) {
                        self.move_dock(&kind, Side::Left);
                    } else if self.sidebars.has_room(Side::Right) {
                        self.move_dock(&kind, Side::Right);
                    }
                    // Both full: the dock can't be placed, so cycling leaves it off.
                }
            },
            Some(LayoutRow::Bar(key)) => {
                let fallback = self
                    .bar
                    .declaration(&key)
                    .map(|declaration| declaration.region)
                    .unwrap_or(crate::bar::BarRegion::BottomRight);
                let next = match self.config.bars.region_for(&key, fallback) {
                    Some(crate::bar::BarRegion::TopRight) => {
                        Some(crate::bar::BarRegion::BottomRight)
                    }
                    Some(crate::bar::BarRegion::BottomRight) => None,
                    None => Some(crate::bar::BarRegion::TopRight),
                };
                self.config.bars.place(&key, next);
                self.bar.clear_geometry();
                config::save(&self.config);
                if let Some(cursor) = self
                    .layout_rows()
                    .iter()
                    .position(|row| matches!(row, LayoutRow::Bar(candidate) if candidate == &key))
                {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.cursor = cursor;
                    }
                }
            }
            _ => self.adjust_layout(cursor, 1),
        }
    }

    /// Cycle the default file-open action (docs/38): read-only → each detected
    /// editor → back. The order matches the `‹ ›` slider in Settings → Layout.
    fn cycle_file_open(&mut self, delta: i32) {
        let mut opts: Vec<String> = vec![config::FILE_OPEN_READONLY.to_string()];
        opts.extend(self.editors.iter().map(|(cmd, _)| cmd.clone()));
        let n = opts.len() as i32;
        if n == 0 {
            return;
        }
        let cur = opts
            .iter()
            .position(|o| *o == self.config.layout.file_open)
            .unwrap_or(0) as i32;
        let next = (((cur + delta) % n + n) % n) as usize;
        self.config.layout.file_open = opts[next].clone();
        config::save(&self.config);
    }

    /// Cycle the Shift/Alt+Enter sequence through [`config::SHIFT_ENTER_CHOICES`].
    fn cycle_shift_enter(&mut self, delta: i32) {
        let opts = config::SHIFT_ENTER_CHOICES;
        let n = opts.len() as i32;
        let cur = opts
            .iter()
            .position(|(k, _, _)| *k == self.config.layout.shift_enter)
            .unwrap_or(0) as i32;
        let next = (((cur + delta) % n + n) % n) as usize;
        self.config.layout.shift_enter = opts[next].0.to_string();
        config::save(&self.config);
    }

    /// The current Shift+Enter choice's display label (the raw keyword if unknown).
    pub fn shift_enter_label(&self) -> String {
        config::SHIFT_ENTER_CHOICES
            .iter()
            .find(|(k, _, _)| *k == self.config.layout.shift_enter)
            .map(|(_, label, _)| label.to_string())
            .unwrap_or_else(|| self.config.layout.shift_enter.clone())
    }

    /// The current file-open choice as a display string: `read-only`, an editor's
    /// label, or the raw command if a configured editor is no longer installed.
    pub fn file_open_label(&self) -> String {
        let choice = &self.config.layout.file_open;
        if choice == config::FILE_OPEN_READONLY {
            return "read-only".to_string();
        }
        self.editors
            .iter()
            .find(|(cmd, _)| cmd == choice)
            .map(|(_, label)| label.clone())
            .unwrap_or_else(|| choice.clone())
    }

    /// Cycle the configured shell (applies to newly opened panes). Windows-only.
    #[cfg(windows)]
    fn cycle_shell(&mut self, delta: i32) {
        let choices = crate::platform::shell_choices();
        let n = choices.len() as i32;
        let cur = choices
            .iter()
            .position(|(k, _)| *k == self.config.shell)
            .unwrap_or(0) as i32;
        let next = (((cur + delta) % n + n) % n) as usize;
        self.config.shell = choices[next].0.to_string();
        config::save(&self.config);
    }

    fn apply_gaps(&mut self) {
        crate::layout::set_gaps(self.config.layout.col_gap, self.config.layout.row_gap);
        config::save(&self.config);
    }

    /// Push the retained-history budget to every live pane. Alacritty's
    /// `Grid::update_history` drops excess rows when its conservative capacity
    /// falls, so lowering this frees memory now rather than only for new panes.
    fn apply_history_budget(&mut self) {
        let bytes = self.config.scrollback_bytes();
        for pane in self.panes.values() {
            pane.set_history_budget(bytes);
        }
    }

    /// General tab ‹ ›/Enter/click on a row: step the file-open choice, flip a
    /// sound toggle, or ring the test chime.
    fn adjust_general(&mut self, cursor: usize, delta: i32) {
        match self.general_rows().get(cursor).copied() {
            Some(GeneralRow::FileOpen) => self.cycle_file_open(delta),
            // Flips config *and* the live tree (docs/38), so it applies at once.
            Some(GeneralRow::FilesShowHidden) => self.toggle_files_hidden(),
            Some(GeneralRow::ShiftEnter) => self.cycle_shift_enter(delta),
            Some(GeneralRow::CheckUpdates) => {
                self.config.check_updates = !self.config.check_updates;
                config::save(&self.config);
            }
            Some(GeneralRow::ResumeFlags) => {
                self.config.resume_launch_flags = !self.config.resume_launch_flags;
                config::save(&self.config);
            }
            Some(GeneralRow::NewPaneToWorkspaceRoot) => {
                self.config.layout.new_pane_to_workspace_root =
                    !self.config.layout.new_pane_to_workspace_root;
                config::save(&self.config);
            }
            Some(GeneralRow::AgentTitle) => {
                self.config.layout.agent_title = !self.config.layout.agent_title;
                config::save(&self.config);
            }
            Some(GeneralRow::SoundDone) => {
                self.config.notifications.sound_on_done = !self.config.notifications.sound_on_done;
                config::save(&self.config);
            }
            Some(GeneralRow::SoundBlocked) => {
                self.config.notifications.sound_on_blocked =
                    !self.config.notifications.sound_on_blocked;
                config::save(&self.config);
            }
            // The Test row fires on Enter/click only (see `settings_activate`) —
            // arrows must not ring it, or holding ‹ › would spam the chime.
            Some(GeneralRow::TestSound) => {}
            None => {}
        }
    }

    /// Play the retro chime once so the user can hear it before turning it on.
    /// Bypasses both sound toggles — it's an explicit manual test.
    fn test_sound(&mut self) {
        self.pending_sound = true;
    }

    /// Toggle an agent's integration hook: install if absent, uninstall if present.
    /// Uninstall removes only luvus's hook — never the agent itself.
    fn install_integration(&mut self, cursor: usize) {
        if let Some(agent) = crate::integration::AGENTS.get(cursor) {
            if crate::integration::is_installed(agent) {
                let _ = crate::integration::uninstall(agent);
            } else {
                let _ = crate::integration::install(agent);
            }
        }
    }
}

fn lang_cursor(code: &str) -> usize {
    crate::i18n::LANGS
        .iter()
        .position(|c| *c == code)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_capture_accepts_safe_non_ctrl_keys_and_exact_modifiers() {
        assert_eq!(
            App::prefix_spec_from_key(&KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)),
            Some("f12".to_string())
        );
        assert_eq!(
            App::prefix_spec_from_key(&KeyEvent::new(
                KeyCode::Char('\\'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            Some("ctrl+alt+\\".to_string())
        );
        assert_eq!(
            App::prefix_spec_from_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE,)),
            None
        );
        assert_eq!(
            App::prefix_spec_from_key(&KeyEvent::new(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some("ctrl+space".to_string())
        );
        assert_eq!(
            App::prefix_spec_from_key(&KeyEvent::new(KeyCode::F(12), KeyModifiers::SUPER,)),
            None
        );
    }

    #[test]
    fn prefix_capture_requires_the_same_chord_twice_before_persisting() {
        let _env = crate::persist::test_env("prefix-capture-confirm");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.open_settings();
        app.settings_set_tab(SettingsTab::Keys);
        app.settings_activate(KEYS_PREFIX_ROW);

        let f12 = KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE);
        app.handle_settings_key(f12);
        assert_eq!(app.config.prefix, "ctrl+space");
        assert!(app.settings.as_ref().is_some_and(|ui| ui.capturing));
        assert_eq!(
            app.settings
                .as_ref()
                .and_then(|ui| ui.prefix_candidate.as_deref()),
            Some("f12")
        );

        app.handle_settings_key(f12);
        assert_eq!(app.config.prefix, "f12");
        assert_eq!(app.prefix, keys::PrefixSpec::parse("f12").unwrap());
        assert!(app.settings.as_ref().is_some_and(|ui| !ui.capturing));
    }

    #[test]
    fn sidebar_widths_are_grouped_in_the_docks_section() {
        let _env = crate::persist::test_env("sidebar-width-settings-order");
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = crate::app::App::new(80, 24, tx).unwrap();
        let rows = app.layout_rows();
        let dock_start = app.dock_section_start();
        let left_visible = rows
            .iter()
            .position(|row| matches!(row, LayoutRow::LeftVisible))
            .unwrap();
        let right_visible = rows
            .iter()
            .position(|row| matches!(row, LayoutRow::RightVisible))
            .unwrap();
        let left_width = rows
            .iter()
            .position(|row| matches!(row, LayoutRow::SidebarWidth))
            .unwrap();
        let right_width = rows
            .iter()
            .position(|row| matches!(row, LayoutRow::RightWidth))
            .unwrap();

        assert_eq!(
            dock_start, left_visible,
            "Docks starts with sidebar controls"
        );
        assert_eq!(right_visible, left_visible + 1);
        assert_eq!(left_width, right_visible + 1);
        assert_eq!(right_width, left_width + 1);
    }

    #[test]
    fn mobile_width_setting_can_disable_and_restore_automatic_layout() {
        let _env = crate::persist::test_env("mobile-width-settings");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        let row = app
            .layout_rows()
            .iter()
            .position(|row| matches!(row, LayoutRow::MobileWidth))
            .expect("the mobile width row is present");

        app.config.layout.mobile_width = 24;
        app.adjust_layout(row, -1);
        assert_eq!(app.config.layout.mobile_width, 0);
        app.adjust_layout(row, 1);
        assert_eq!(app.config.layout.mobile_width, 24);
    }

    #[test]
    fn bar_settings_rows_stay_stable_when_placement_changes() {
        let _env = crate::persist::test_env("bar-settings-stable");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        let declaration = crate::bar::BarDeclaration {
            key: crate::bar::BarWidgetKey::new("example", "ci"),
            title: "CI status".into(),
            region: crate::bar::BarRegion::TopRight,
            priority: 50,
        };
        app.bar
            .declarations
            .insert(declaration.key.canonical(), declaration);
        let expected = app.bar_setting_keys();

        for region in [
            Some(crate::bar::BarRegion::TopRight),
            Some(crate::bar::BarRegion::BottomRight),
            None,
        ] {
            app.config.bars.place("example:ci", region);
            assert_eq!(app.bar_setting_keys(), expected);
        }
    }

    #[test]
    fn dock_settings_rows_stay_stable_when_placement_changes() {
        let _env = crate::persist::test_env("dock-settings-stable");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        let alpha = DockKind::Module("example:alpha".into());
        let beta = DockKind::Module("example:beta".into());
        assert!(app.move_dock(&alpha, Side::Right));
        assert!(app.move_dock(&beta, Side::Right));
        let expected = app.available_docks();

        assert!(app.move_dock(&alpha, Side::Left));
        assert_eq!(app.available_docks(), expected);
        app.unmount_dock(&alpha);
        assert_eq!(app.available_docks(), expected);
    }

    #[test]
    fn clicking_a_visible_placement_button_keeps_the_layout_viewport() {
        use ratatui::{backend::TestBackend, Terminal};

        let _env = crate::persist::test_env("settings-placement-scroll");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(100, 24, tx).unwrap();
        let declaration = crate::bar::BarDeclaration {
            key: crate::bar::BarWidgetKey::new("example", "status"),
            title: "Example status".into(),
            region: crate::bar::BarRegion::BottomRight,
            priority: 50,
        };
        app.bar
            .declarations
            .insert(declaration.key.canonical(), declaration);
        app.open_settings();
        let last = app.layout_rows().len().saturating_sub(1);
        if let Some(settings) = app.settings.as_mut() {
            settings.tab = SettingsTab::Layout;
            settings.cursor = last;
        }

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let before = app.settings.as_ref().unwrap().layout_scroll;
        assert!(before > 0, "the regression requires a scrolled Layout list");

        let rows = app.layout_rows();
        let (_, _, button) = app
            .settings_arrow_rects
            .iter()
            .find(|(index, delta, _)| {
                *delta == 2
                    && *index != last
                    && matches!(
                        rows.get(*index),
                        Some(LayoutRow::Dock(_) | LayoutRow::Bar(_))
                    )
            })
            .copied()
            .expect("a visible placement button above the selected row");
        app.handle_settings_click(button.x, button.y);
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        assert_eq!(
            app.settings.as_ref().unwrap().layout_scroll,
            before,
            "clicking a visible placement button must not re-anchor the modal"
        );
    }

    #[test]
    fn diff_marker_setting_cycles_live_and_persists() {
        let _env = crate::persist::test_env("diff-marker-style");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.open_settings();
        if let Some(settings) = app.settings.as_mut() {
            settings.tab = SettingsTab::Layout;
        }
        let row = app
            .layout_rows()
            .iter()
            .position(|row| matches!(row, LayoutRow::DiffMarkers))
            .expect("the DIFF marker row is present");

        assert_eq!(
            app.config.layout.diff_marker_style,
            crate::diff::DiffMarkerStyle::Symbols
        );
        app.settings_adjust(row, 1);
        assert_eq!(
            app.config.layout.diff_marker_style,
            crate::diff::DiffMarkerStyle::Bars
        );
        assert_eq!(
            crate::config::load().layout.diff_marker_style,
            crate::diff::DiffMarkerStyle::Bars,
            "the selection survives restart"
        );
        app.settings_adjust(row, 1);
        assert_eq!(
            app.config.layout.diff_marker_style,
            crate::diff::DiffMarkerStyle::Both
        );
        app.settings_adjust(row, -1);
        assert_eq!(
            app.config.layout.diff_marker_style,
            crate::diff::DiffMarkerStyle::Bars,
            "reverse navigation follows the same ordering"
        );
    }

    #[test]
    fn diff_color_setting_cycles_live_and_persists() {
        let _env = crate::persist::test_env("diff-color-mode");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.open_settings();
        if let Some(settings) = app.settings.as_mut() {
            settings.tab = SettingsTab::Layout;
        }
        let row = app
            .layout_rows()
            .iter()
            .position(|row| matches!(row, LayoutRow::DiffColors))
            .expect("the DIFF color row is present");

        assert_eq!(
            app.config.layout.diff_color_mode,
            crate::diff::DiffColorMode::Theme
        );
        app.settings_adjust(row, 1);
        assert_eq!(
            app.config.layout.diff_color_mode,
            crate::diff::DiffColorMode::Standard
        );
        assert_eq!(
            crate::config::load().layout.diff_color_mode,
            crate::diff::DiffColorMode::Standard,
            "the color choice survives restart"
        );
        app.settings_adjust(row, -1);
        assert_eq!(
            app.config.layout.diff_color_mode,
            crate::diff::DiffColorMode::Theme
        );
    }

    /// The docs/62 switch: whether resume replays each agent's own CLI options,
    /// or falls back to the plain resume command luvus used before the feature.
    ///
    /// **Off by default** — a remembered option outlives the session it was set
    /// for, and some of them widen what the agent may do, so it is opt-in.
    #[test]
    fn resume_flags_toggle_persists() {
        let _env = crate::persist::test_env("resume-flags");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        assert!(!app.config.resume_launch_flags, "off by default: opt-in");

        app.open_settings();
        if let Some(ui) = app.settings.as_mut() {
            ui.tab = SettingsTab::General;
        }
        let row = app
            .general_rows()
            .iter()
            .position(|r| *r == GeneralRow::ResumeFlags)
            .expect("the row is on the General tab");
        // It sits with the general options, above the Notify divider.
        assert!(row < app.general_section_start());

        app.adjust_general(row, 1);
        assert!(app.config.resume_launch_flags, "the toggle flipped");
        assert!(
            crate::config::load().resume_launch_flags,
            "and it was saved"
        );

        app.adjust_general(row, 1);
        assert!(!app.config.resume_launch_flags, "toggles back");

        // A config written before this field existed loads as off: an existing
        // user is never opted in behind their back.
        let old: crate::config::Config = serde_json::from_str("{}").unwrap();
        assert!(!old.resume_launch_flags);
    }

    /// The "Open new pane/tab at workspace root" toggle is opt-in: off by
    /// default (a new tab/split inherits the focused pane's cwd), and flipping
    /// it in Settings → General persists.
    #[test]
    fn new_pane_to_workspace_root_toggle_persists() {
        let _env = crate::persist::test_env("new-pane-workspace-root");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        assert!(
            !app.config.layout.new_pane_to_workspace_root,
            "off by default: a new tab/split inherits the focused pane's cwd"
        );

        app.open_settings();
        if let Some(ui) = app.settings.as_mut() {
            ui.tab = SettingsTab::General;
        }
        let row = app
            .general_rows()
            .iter()
            .position(|r| *r == GeneralRow::NewPaneToWorkspaceRoot)
            .expect("the row is on the General tab");
        // It sits with the general options, above the Notify divider.
        assert!(row < app.general_section_start());

        app.adjust_general(row, 1);
        assert!(
            app.config.layout.new_pane_to_workspace_root,
            "the toggle flipped"
        );
        assert!(
            crate::config::load().layout.new_pane_to_workspace_root,
            "and it was saved"
        );

        app.adjust_general(row, 1);
        assert!(
            !app.config.layout.new_pane_to_workspace_root,
            "toggles back"
        );

        // A config written before this field existed loads as off: the inherit
        // behavior stays the default for an existing user.
        let old: crate::config::Config = serde_json::from_str("{}").unwrap();
        assert!(!old.layout.new_pane_to_workspace_root);
    }

    // The General tab is the file-open chooser plus the Notifications section:
    // the two sound toggles (persisted) and a Test row that rings the chime
    // immediately, regardless of the toggles.
    #[test]
    fn general_tab_toggles_sounds_and_tests_the_chime() {
        let _env = crate::persist::test_env("general-tab");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.open_settings();
        if let Some(ui) = app.settings.as_mut() {
            ui.tab = SettingsTab::General;
        }
        assert_eq!(app.settings_rows(SettingsTab::General), 10);
        let rows = app.general_rows();
        assert_eq!(rows[0], GeneralRow::FileOpen, "file-open leads the tab");

        let done = rows
            .iter()
            .position(|r| *r == GeneralRow::SoundDone)
            .unwrap();
        let blocked = rows
            .iter()
            .position(|r| *r == GeneralRow::SoundBlocked)
            .unwrap();
        let test = rows
            .iter()
            .position(|r| *r == GeneralRow::TestSound)
            .unwrap();

        app.settings_activate(done);
        assert!(app.config.notifications.sound_on_done, "toggles done");
        app.settings_activate(blocked);
        assert!(app.config.notifications.sound_on_blocked, "toggles blocked");

        assert!(!app.pending_sound);
        // Arrows must NOT ring the chime (only Enter/click does).
        app.settings_adjust(test, 1);
        assert!(!app.pending_sound, "‹ › on the Test row does not ring");
        app.settings_activate(test);
        assert!(app.pending_sound, "the Test row rings the chime");
    }

    /// The General tab renders the file-open chooser, then a `Notify` section
    /// divider, then the sound rows — the Docks-section treatment (docs/15).
    #[test]
    fn general_tab_renders_file_open_then_a_notify_section() {
        use ratatui::{backend::TestBackend, Terminal};
        let _env = crate::persist::test_env("general-render");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(120, 40, tx).unwrap();
        app.editors = vec![("vim".into(), "vim".into())];
        app.open_settings();
        if let Some(ui) = app.settings.as_mut() {
            ui.tab = SettingsTab::General;
        }
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let text: Vec<String> = (0..buf.area.height)
            .map(|r| {
                (0..buf.area.width)
                    .map(|c| buf.cell((c, r)).map(|x| x.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect();
        let all = text.join("\n");
        assert!(all.contains("General"), "the General tab is in the strip");
        assert!(all.contains("Open files with"), "file-open row drawn");
        assert!(all.contains("read-only"), "its current value drawn");
        assert!(
            all.contains("Remember CLI option"),
            "the resume switch drawn"
        );
        assert!(all.contains("Notify"), "the notifications section divider");

        // Order: file-open row, the resume switch, then the divider and sounds.
        let row_of = |needle: &str| text.iter().position(|l| l.contains(needle));
        let (fo, res, div, snd) = (
            row_of("Open files with"),
            row_of("Remember CLI option"),
            row_of("Notify"),
            row_of("Test sound"),
        );
        assert!(
            fo < res && res < div && div < snd,
            "file-open → resume switch → divider → sounds"
        );
        // A blank line separates the options from the section header.
        let (res, div) = (res.unwrap(), div.unwrap());
        assert!(div >= res + 2, "a blank gap sits above the section divider");
    }

    /// Notifications is no longer its own tab: General leads the tab strip and
    /// the sound settings live inside it.
    #[test]
    fn general_replaces_the_notifications_tab() {
        assert_eq!(
            SettingsTab::ALL[0],
            SettingsTab::General,
            "General is first"
        );
        assert_eq!(SettingsTab::ALL[1], SettingsTab::Theme, "before Theme");
        assert_eq!(SettingsTab::ALL.len(), 7, "still seven tabs");
    }

    #[test]
    fn settings_tab_icons_have_consistent_terminal_width() {
        assert_eq!(SettingsTab::General.icon(), "◆");
        assert_eq!(SettingsTab::Layout.icon(), "▣");
        assert_eq!(SettingsTab::Integrations.icon(), "☆");
        for tab in SettingsTab::ALL {
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(tab.icon()),
                1,
                "{tab:?} icon must occupy one terminal column"
            );
        }
    }

    /// The General tab's "Open files with" slider cycles read-only → each detected
    /// editor → back, and steps backward with wraparound.
    #[test]
    fn general_file_open_cycles_through_editors() {
        let _env = crate::persist::test_env("file-open-cycle");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.editors = vec![("vim".into(), "vim".into()), ("nano".into(), "nano".into())];
        app.open_settings();
        if let Some(ui) = app.settings.as_mut() {
            ui.tab = SettingsTab::General;
        }
        let idx = app
            .general_rows()
            .iter()
            .position(|r| *r == GeneralRow::FileOpen)
            .expect("the General tab has a file-open row");

        assert_eq!(app.config.layout.file_open, "readonly", "starts read-only");
        app.settings_adjust(idx, 1);
        assert_eq!(app.config.layout.file_open, "vim");
        app.settings_adjust(idx, 1);
        assert_eq!(app.config.layout.file_open, "nano");
        app.settings_adjust(idx, 1);
        assert_eq!(
            app.config.layout.file_open, "readonly",
            "wraps back to read-only"
        );
        app.settings_adjust(idx, -1);
        assert_eq!(
            app.config.layout.file_open, "nano",
            "steps backward with wrap"
        );
    }

    /// The General tab's Shift+Enter chooser cycles through the known sequences
    /// and drives the bytes `encode_key` forwards.
    #[test]
    fn general_shift_enter_cycles_and_drives_the_bytes() {
        let _env = crate::persist::test_env("shift-enter-cycle");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.open_settings();
        if let Some(ui) = app.settings.as_mut() {
            ui.tab = SettingsTab::General;
        }
        let idx = app
            .general_rows()
            .iter()
            .position(|r| *r == GeneralRow::ShiftEnter)
            .expect("the General tab has a Shift+Enter row");

        assert_eq!(app.config.layout.shift_enter, "esc-cr", "starts at ESC CR");
        assert_eq!(app.config.shift_enter_bytes(), b"\x1b\r");
        app.settings_adjust(idx, 1);
        assert_eq!(app.config.layout.shift_enter, "lf", "steps to LF");
        assert_eq!(app.config.shift_enter_bytes(), b"\n");
        // Backward from the first entry wraps to the last.
        app.settings_adjust(idx, -1);
        app.settings_adjust(idx, -1);
        let last = config::SHIFT_ENTER_CHOICES.last().unwrap().0;
        assert_eq!(app.config.layout.shift_enter, last, "wraps to the last");
    }

    #[test]
    fn missing_configured_theme_falls_back_without_erasing_the_id() {
        let _env = crate::persist::test_env("missing-custom-theme");
        crate::persist::ensure_config_dir();
        let mut config = crate::config::load();
        config.theme = "temporarily-missing".to_string();
        crate::config::save(&config);
        let (tx, _rx) = std::sync::mpsc::channel();
        let app = crate::app::App::new(80, 24, tx).unwrap();
        assert_eq!(app.config.theme, "temporarily-missing");
        assert_eq!(
            app.theme,
            crate::ui::theme::Theme::quattro_rally(),
            "rendering uses the safe default until the file returns"
        );
    }

    #[test]
    fn off_loop_registry_reload_swaps_the_dynamic_theme_list() {
        let _env = crate::persist::test_env("theme-reload-event");
        crate::persist::ensure_config_dir();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        assert!(app.theme_registry.get("reloaded-theme").is_none());

        let source = crate::persist::config_dir().join("reloaded-theme.toml");
        crate::theme::install::init(&source, "reloaded-theme", Some("noir")).unwrap();
        crate::theme::install::install(source.to_str().unwrap(), true).unwrap();
        let (reply, response) = std::sync::mpsc::channel();
        app.handle_event(crate::event::AppEvent::ThemeReloaded {
            id: "reload-1".to_string(),
            registry: crate::theme::ThemeRegistry::load(),
            reply,
        });
        assert!(app.theme_registry.get("reloaded-theme").is_some());
        assert!(response.recv().unwrap().contains("themes_reloaded"));
    }

    #[test]
    fn installed_theme_remove_action_is_scoped_guarded_and_off_loop() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::time::Duration;

        let _env = crate::persist::test_env("installed-theme-remove-settings");
        let source = crate::persist::ensure_config_dir().join("custom-remove.toml");
        crate::theme::install::init(&source, "custom-remove", None).unwrap();
        let installed = crate::theme::install::install(source.to_str().unwrap(), true).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.apply_theme("terminal");
        app.open_settings();
        app.settings_set_tab(SettingsTab::Theme);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        assert_eq!(
            app.settings_theme_remove_rects
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["custom-remove"],
            "only installed files expose removal; bundled and virtual themes do not"
        );
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("installed") && screen.contains("remove"));

        app.apply_theme("custom-remove");
        app.settings_set_tab(SettingsTab::Theme);
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let remove = app
            .settings_theme_remove_rects
            .iter()
            .find(|(id, _)| id == "custom-remove")
            .unwrap()
            .1;
        app.handle_settings_click(remove.x, remove.y);
        assert_eq!(
            app.config.theme,
            theme::THEMES[0],
            "removing the active file switches to the bundled default first"
        );
        assert!(installed.path.exists(), "filesystem removal stays off-loop");
        assert!(
            app.settings_theme_remove_rects
                .iter()
                .all(|(id, _)| id != "custom-remove"),
            "the action is retired before the worker completes"
        );
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        assert!(
            app.settings_theme_remove_rects
                .iter()
                .all(|(id, _)| id != "custom-remove"),
            "a repaint cannot recreate an action while removal is pending"
        );

        let event = loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                event @ crate::event::AppEvent::ThemeUninstalled { .. } => break event,
                other => {
                    app.handle_event(other);
                }
            }
        };
        app.handle_event(event);
        assert!(!installed.path.exists());
        assert!(app.theme_registry.get("custom-remove").is_none());
        assert!(app
            .settings_theme_remove_rects
            .iter()
            .all(|(id, _)| id != "custom-remove"));
        assert!(app
            .toast
            .as_ref()
            .is_some_and(|(message, _)| message == "Removed theme custom-remove"));
    }

    #[test]
    fn failed_theme_removal_restores_the_previous_selection() {
        let _env = crate::persist::test_env("failed-theme-remove-settings");
        let source = crate::persist::ensure_config_dir().join("custom-restore.toml");
        crate::theme::install::init(&source, "custom-restore", None).unwrap();
        crate::theme::install::install(source.to_str().unwrap(), true).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.apply_theme("custom-restore");
        let previous = app.config.theme.clone();
        app.apply_theme(theme::THEMES[0]);
        app.pending_theme_uninstalls.insert(
            "custom-restore".into(),
            Some((previous, app.theme_selection_revision)),
        );
        app.finish_theme_uninstall(
            "custom-restore".into(),
            Err("still required by child-theme".into()),
        );

        assert_eq!(app.config.theme, "custom-restore");
        assert!(app
            .toast
            .as_ref()
            .is_some_and(|(message, _)| message.contains("still required by child-theme")));
    }

    #[test]
    fn enter_routes_an_installed_theme_through_removal() {
        let _env = crate::persist::test_env("installed-theme-enter-remove");
        let source = crate::persist::ensure_config_dir().join("custom-enter.toml");
        crate::theme::install::init(&source, "custom-enter", None).unwrap();
        crate::theme::install::install(source.to_str().unwrap(), true).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.apply_theme("custom-enter");
        app.open_settings();
        app.settings_set_tab(SettingsTab::Theme);
        let index = app.theme_registry.index_of("custom-enter").unwrap();
        app.settings_activate(index);
        assert!(!app.theme_uninstall_pending("custom-enter"));
        app.settings_enter_theme(index);

        assert_eq!(app.config.theme, theme::THEMES[0]);
        assert!(app.theme_uninstall_pending("custom-enter"));
        let event = loop {
            match rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap() {
                event @ crate::event::AppEvent::ThemeUninstalled { .. } => break event,
                other => {
                    app.handle_event(other);
                }
            }
        };
        app.handle_event(event);
        assert!(app.theme_registry.get("custom-enter").is_none());
    }

    #[test]
    fn failed_theme_removal_preserves_a_newer_selection() {
        let _env = crate::persist::test_env("failed-theme-remove-newer-selection");
        let source = crate::persist::ensure_config_dir().join("custom-pending.toml");
        crate::theme::install::init(&source, "custom-pending", None).unwrap();
        crate::theme::install::install(source.to_str().unwrap(), true).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        app.apply_theme("custom-pending");
        let previous = app.config.theme.clone();
        app.apply_theme(theme::THEMES[0]);
        let fallback_revision = app.theme_selection_revision;
        app.pending_theme_uninstalls
            .insert("custom-pending".into(), Some((previous, fallback_revision)));
        app.apply_theme("noir");

        app.finish_theme_uninstall(
            "custom-pending".into(),
            Err("still required by child-theme".into()),
        );

        assert_eq!(app.config.theme, "noir");
        assert!(!app.theme_uninstall_pending("custom-pending"));
    }

    #[test]
    fn installed_theme_is_selectable_and_applies_live() {
        let _env = crate::persist::test_env("installed-theme-settings");
        let source = crate::persist::ensure_config_dir().join("custom-live.toml");
        crate::theme::install::init(&source, "custom-live", None).unwrap();
        crate::theme::install::install(source.to_str().unwrap(), true).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(80, 24, tx).unwrap();
        let index = app
            .theme_registry
            .index_of("custom-live")
            .expect("installed theme appears in the dynamic registry");
        assert!(
            index < app.theme_registry.entries().len() - 1,
            "custom themes appear before the virtual terminal theme"
        );
        let expected = app.theme_registry.theme("custom-live").unwrap();
        app.apply_theme("custom-live");
        assert_eq!(app.config.theme, "custom-live");
        assert_eq!(app.theme, expected);
    }
}
