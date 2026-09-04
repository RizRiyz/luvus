//! On-demand named-session switcher shared by desktop and mobile chrome.
//!
//! Discovery and server startup are explicit user actions and run on short-lived
//! workers. The idle app loop retains only small display rows and hit geometry.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

use super::App;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedSessionRow {
    pub name: String,
    pub running: bool,
    pub current: bool,
}

#[derive(Debug)]
pub struct NamedSessionMenu {
    pub generation: u64,
    pub rows: Vec<NamedSessionRow>,
    pub cursor: usize,
    pub scroll: usize,
    pub loading: bool,
    pub prompt: Option<String>,
    pub error: Option<String>,
    pub preparing: bool,
}

#[derive(Debug)]
pub enum NamedSessionOpenError {
    Exists,
    Failed(String),
}

impl App {
    pub fn apply_named_session_stopped(
        &mut self,
        generation: u64,
        name: String,
        result: Result<(), String>,
    ) {
        // Borrow-check: need to handle show_toast after releasing menu borrow.
        let is_closed = self.named_session_menu.is_none();
        if is_closed {
            match result {
                Ok(()) => self.show_toast(format!("stopped {name}")),
                Err(err) => self.show_toast(format!("could not stop {name}: {err}")),
            }
            return;
        }
        let generation_matches = self
            .named_session_menu
            .as_ref()
            .is_some_and(|m| m.generation == generation);
        if !generation_matches {
            return;
        }
        // Capture values before mutable borrow for toast.
        let toast = match &result {
            Ok(()) => format!("stopped {name}"),
            Err(_) => String::new(),
        };
        let (gen, had_error) = {
            let menu = self.named_session_menu.as_mut().unwrap();
            match result {
                Ok(()) => {
                    if let Some(pos) = menu.rows.iter().position(|r| r.name == name) {
                        menu.rows.remove(pos);
                        let count = menu.rows.len() + 1;
                        if menu.cursor >= count {
                            menu.cursor = count.saturating_sub(1);
                        }
                    }
                    (menu.generation, false)
                }
                Err(err) => {
                    menu.error = Some(err);
                    (0, true)
                }
            }
        };
        if !had_error {
            self.show_toast(toast);
            let tx = self.app_tx.clone();
            std::thread::spawn(move || {
                let result = crate::session::list_sessions().map_err(|e| e.to_string());
                let _ = tx.send(crate::event::AppEvent::NamedSessionsLoaded {
                    generation: gen,
                    result,
                });
            });
        }
    }

    pub fn open_named_session_menu(&mut self) {
        if !self.server_mode {
            self.show_toast(self.catalog.session_open_failed);
            return;
        }
        self.switcher = false;
        self.named_session_generation = self.named_session_generation.wrapping_add(1);
        let generation = self.named_session_generation;
        self.named_session_menu = Some(NamedSessionMenu {
            generation,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            loading: true,
            prompt: None,
            error: None,
            preparing: false,
        });
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = crate::session::list_sessions().map_err(|err| err.to_string());
            let _ = tx.send(crate::event::AppEvent::NamedSessionsLoaded { generation, result });
        });
    }

    pub fn close_named_session_menu(&mut self) {
        self.named_session_generation = self.named_session_generation.wrapping_add(1);
        self.named_session_menu = None;
        self.session_menu = None;
    }

    pub fn apply_named_sessions_loaded(
        &mut self,
        generation: u64,
        result: Result<Vec<crate::session::SessionInfo>, String>,
    ) {
        let current = crate::session::display_name();
        let Some(menu) = self.named_session_menu.as_mut() else {
            return;
        };
        if menu.generation != generation {
            return;
        }
        menu.loading = false;
        match result {
            Ok(sessions) => {
                menu.rows = session_rows(sessions, &current);
                menu.cursor = menu
                    .rows
                    .iter()
                    .position(|row| row.current)
                    .map_or(0, |index| index + 1);
            }
            Err(error) => {
                menu.error = Some(format!("{}: {error}", self.catalog.session_open_failed))
            }
        }
    }

    pub fn apply_named_session_prepared(
        &mut self,
        generation: u64,
        name: String,
        result: Result<(), NamedSessionOpenError>,
    ) {
        let Some(menu) = self.named_session_menu.as_mut() else {
            return;
        };
        if menu.generation != generation {
            return;
        }
        menu.preparing = false;
        match result {
            Ok(()) => {
                self.named_session_menu = None;
                self.named_session_generation = self.named_session_generation.wrapping_add(1);
                self.pending_session_switch = Some(name);
            }
            Err(NamedSessionOpenError::Exists) => {
                menu.error = Some(self.catalog.session_exists.to_string());
            }
            Err(NamedSessionOpenError::Failed(error)) => {
                menu.error = Some(format!("{}: {error}", self.catalog.session_open_failed));
            }
        }
    }

    pub fn named_session_key(&mut self, key: KeyEvent) {
        let prompt_open = self
            .named_session_menu
            .as_ref()
            .is_some_and(|menu| menu.prompt.is_some());
        if prompt_open {
            match key.code {
                KeyCode::Esc => {
                    self.named_session_generation = self.named_session_generation.wrapping_add(1);
                    if let Some(menu) = self.named_session_menu.as_mut() {
                        menu.generation = self.named_session_generation;
                        menu.prompt = None;
                        menu.error = None;
                        menu.preparing = false;
                    }
                }
                KeyCode::Enter => self.submit_named_session_prompt(),
                KeyCode::Backspace => {
                    if let Some(menu) = self.named_session_menu.as_mut() {
                        if !menu.preparing {
                            menu.prompt.as_mut().map(String::pop);
                            menu.error = None;
                        }
                    }
                }
                KeyCode::Char(character)
                    if !super::keys::is_ctrl_chord(key.modifiers) && !character.is_control() =>
                {
                    if let Some(menu) = self.named_session_menu.as_mut() {
                        if !menu.preparing {
                            if let Some(prompt) = menu.prompt.as_mut() {
                                if prompt.len() < 64 && is_session_name_character(character) {
                                    prompt.push(character);
                                }
                            }
                            menu.error = None;
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        let count = self
            .named_session_menu
            .as_ref()
            .map_or(0, |menu| menu.rows.len() + 1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_named_session_menu(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(menu) = self.named_session_menu.as_mut() {
                    menu.cursor = menu.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(menu) = self.named_session_menu.as_mut() {
                    if count > 0 {
                        menu.cursor = (menu.cursor + 1).min(count - 1);
                    }
                }
            }
            KeyCode::Home => {
                if let Some(menu) = self.named_session_menu.as_mut() {
                    menu.cursor = 0;
                }
            }
            KeyCode::End => {
                if let Some(menu) = self.named_session_menu.as_mut() {
                    menu.cursor = count.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                let cursor = self
                    .named_session_menu
                    .as_ref()
                    .map_or(0, |menu| menu.cursor);
                self.activate_named_session_row(cursor);
            }
            _ => {}
        }
    }

    pub fn paste_named_session_prompt(&mut self, text: &str) -> bool {
        let Some(menu) = self.named_session_menu.as_mut() else {
            return false;
        };
        let Some(prompt) = menu.prompt.as_mut() else {
            return true;
        };
        if menu.preparing {
            return true;
        }
        for character in text
            .chars()
            .filter(|character| is_session_name_character(*character))
        {
            if prompt.len() >= 64 {
                break;
            }
            prompt.push(character);
        }
        menu.error = None;
        true
    }

    pub fn named_session_click(&mut self, column: u16, row: u16) {
        let hit = |rect: ratatui::layout::Rect| {
            column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
        };
        if self.named_session_close_rect.is_some_and(hit) {
            self.close_named_session_menu();
            return;
        }
        if let Some(index) = self
            .named_session_row_rects
            .iter()
            .find(|(_, rect)| hit(*rect))
            .map(|(index, _)| *index)
        {
            self.activate_named_session_row(index);
            return;
        }
        if !self.compact && !self.named_session_menu_rect.is_some_and(hit) {
            self.close_named_session_menu();
        }
    }

    pub fn move_named_session_cursor(&mut self, delta: i32) {
        let Some(menu) = self.named_session_menu.as_mut() else {
            return;
        };
        if menu.prompt.is_some() {
            return;
        }
        let count = menu.rows.len() + 1;
        menu.cursor =
            (menu.cursor as i32 + delta).clamp(0, count.saturating_sub(1) as i32) as usize;
    }

    fn activate_named_session_row(&mut self, index: usize) {
        let Some(menu) = self.named_session_menu.as_mut() else {
            return;
        };
        if menu.loading || menu.preparing {
            return;
        }
        if index == 0 {
            menu.prompt = Some(String::new());
            menu.error = None;
            return;
        }
        let Some((name, current)) = menu
            .rows
            .get(index - 1)
            .map(|row| (row.name.clone(), row.current))
        else {
            return;
        };
        if current {
            self.close_named_session_menu();
            return;
        }
        self.prepare_named_session(name, false);
    }

    fn submit_named_session_prompt(&mut self) {
        let Some(menu) = self.named_session_menu.as_mut() else {
            return;
        };
        if menu.preparing {
            return;
        }
        let name = menu
            .prompt
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if crate::session::validate_name(&name).is_err() {
            menu.error = Some(self.catalog.session_name_hint.to_string());
            return;
        }
        self.prepare_named_session(name, true);
    }

    fn prepare_named_session(&mut self, name: String, must_be_new: bool) {
        let Some(menu) = self.named_session_menu.as_mut() else {
            return;
        };
        menu.preparing = true;
        menu.error = None;
        let generation = menu.generation;
        let tx = self.app_tx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                if must_be_new {
                    let sessions = crate::session::list_sessions()
                        .map_err(|err| NamedSessionOpenError::Failed(err.to_string()))?;
                    if sessions.iter().any(|session| session.name == name) {
                        return Err(NamedSessionOpenError::Exists);
                    }
                }
                crate::session::start_client_session(&name)
                    .map(|_| ())
                    .map_err(NamedSessionOpenError::Failed)
            })();
            let _ = tx.send(crate::event::AppEvent::NamedSessionPrepared {
                generation,
                name,
                result,
            });
        });
    }
}

fn session_rows(sessions: Vec<crate::session::SessionInfo>, current: &str) -> Vec<NamedSessionRow> {
    let mut rows: Vec<_> = sessions
        .into_iter()
        .map(|session| NamedSessionRow {
            current: session.name == current,
            name: session.name,
            running: session.running,
        })
        .collect();
    if !rows.iter().any(|row| row.current) {
        rows.push(NamedSessionRow {
            name: current.to_string(),
            running: true,
            current: true,
        });
    }
    rows.sort_by(|left, right| {
        (!left.current, !left.running, left.name.to_ascii_lowercase()).cmp(&(
            !right.current,
            !right.running,
            right.name.to_ascii_lowercase(),
        ))
    });
    rows
}

fn is_session_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::{session_rows, NamedSessionMenu, NamedSessionRow};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn info(name: &str, running: bool) -> crate::session::SessionInfo {
        let mut info = crate::session::session_info(None);
        info.name = name.to_string();
        info.running = running;
        info
    }

    #[test]
    fn rows_put_current_then_running_then_stopped() {
        let rows = session_rows(
            vec![
                info("z-stopped", false),
                info("b-running", true),
                info("active", true),
                info("a-running", true),
            ],
            "active",
        );
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["active", "a-running", "b-running", "z-stopped"]);
    }

    #[test]
    fn rows_restore_a_missing_current_session() {
        let rows = session_rows(vec![info("default", true)], "other");
        assert_eq!(rows[0].name, "other");
        assert!(rows[0].current);
        assert!(rows[0].running);
    }

    #[test]
    fn loaded_menu_selects_the_current_session_not_new() {
        let _env = crate::persist::test_env("named-session-menu-current");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(100, 30, tx).unwrap();
        app.named_session_menu = Some(NamedSessionMenu {
            generation: 7,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            loading: true,
            prompt: None,
            error: None,
            preparing: false,
        });
        app.apply_named_sessions_loaded(7, Ok(vec![info("stopped", false), info("default", true)]));
        let menu = app.named_session_menu.as_ref().unwrap();
        assert_eq!(menu.cursor, 1, "row zero is New; current is row one");
        assert!(menu.rows[0].current);
    }

    #[test]
    fn keyboard_navigation_supports_vim_edges_and_q() {
        let _env = crate::persist::test_env("named-session-menu-navigation");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(100, 30, tx).unwrap();
        app.named_session_menu = Some(NamedSessionMenu {
            generation: 1,
            rows: vec![
                NamedSessionRow {
                    name: "default".into(),
                    running: true,
                    current: true,
                },
                NamedSessionRow {
                    name: "review".into(),
                    running: false,
                    current: false,
                },
            ],
            cursor: 1,
            scroll: 0,
            loading: false,
            prompt: None,
            error: None,
            preparing: false,
        });

        app.named_session_key(key(KeyCode::Char('k')));
        assert_eq!(app.named_session_menu.as_ref().unwrap().cursor, 0);
        app.named_session_key(key(KeyCode::Char('k')));
        assert_eq!(app.named_session_menu.as_ref().unwrap().cursor, 0);
        app.named_session_key(key(KeyCode::End));
        assert_eq!(app.named_session_menu.as_ref().unwrap().cursor, 2);
        app.named_session_key(key(KeyCode::Char('j')));
        assert_eq!(app.named_session_menu.as_ref().unwrap().cursor, 2);
        app.named_session_key(key(KeyCode::Home));
        assert_eq!(app.named_session_menu.as_ref().unwrap().cursor, 0);
        app.named_session_key(key(KeyCode::Char('j')));
        assert_eq!(app.named_session_menu.as_ref().unwrap().cursor, 1);
        app.named_session_key(key(KeyCode::Char('q')));
        assert!(app.named_session_menu.is_none());
    }

    #[test]
    fn q_remains_text_inside_the_new_session_prompt() {
        let _env = crate::persist::test_env("named-session-menu-prompt-q");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(100, 30, tx).unwrap();
        app.named_session_menu = Some(NamedSessionMenu {
            generation: 1,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            loading: false,
            prompt: Some(String::new()),
            error: None,
            preparing: false,
        });

        app.named_session_key(key(KeyCode::Char('q')));

        assert_eq!(
            app.named_session_menu
                .as_ref()
                .and_then(|menu| menu.prompt.as_deref()),
            Some("q")
        );
    }

    #[test]
    fn stale_preparation_cannot_handoff_after_the_prompt_is_cancelled() {
        let _env = crate::persist::test_env("named-session-menu-stale");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(100, 30, tx).unwrap();
        app.named_session_menu = Some(NamedSessionMenu {
            generation: 4,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            loading: false,
            prompt: Some("review".into()),
            error: None,
            preparing: true,
        });
        app.named_session_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Esc,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        app.apply_named_session_prepared(4, "review".into(), Ok(()));
        assert!(app.pending_session_switch.is_none());
        assert!(app.named_session_menu.is_some());
    }

    #[test]
    fn open_session_menu_creating_flag_blocks_stop_until_ready() {
        let _env = crate::persist::test_env("named-session-menu-preparing-guard");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = crate::app::App::new(100, 30, tx).unwrap();
        app.named_session_menu = Some(NamedSessionMenu {
            generation: 1,
            rows: vec![NamedSessionRow {
                name: "other".into(),
                running: true,
                current: false,
            }],
            cursor: 1,
            scroll: 0,
            loading: false,
            prompt: None,
            error: None,
            preparing: true,
        });
        // Right-click guard is row.running && !row.current && !menu.preparing.
        let menu = app.named_session_menu.as_ref().unwrap();
        let row = &menu.rows[0];
        assert!(row.running && !row.current);
        assert!(menu.preparing);
        assert!(!(row.running && !row.current && !menu.preparing));
        // Direct guard on open_session_menu: ineligible rows must clear stale menu.
        app.session_menu = Some(crate::app::SessionMenu {
            name: "old".into(),
            anchor: (0, 0),
            items: Vec::new(),
        });
        app.open_session_menu("current".into(), 0, 0, true, true);
        assert!(
            app.session_menu.is_none(),
            "current row must clear stale Stop menu"
        );
        app.session_menu = Some(crate::app::SessionMenu {
            name: "old".into(),
            anchor: (0, 0),
            items: Vec::new(),
        });
        app.open_session_menu("stopped".into(), 0, 0, false, false);
        assert!(
            app.session_menu.is_none(),
            "stopped row must clear stale Stop menu"
        );
        // Eligible row still opens.
        app.open_session_menu("other".into(), 5, 6, true, false);
        assert!(app.session_menu.is_some());
        assert_eq!(app.session_menu.as_ref().unwrap().name, "other");
    }
}
