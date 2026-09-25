//! Focused Commander integration tests in the App privacy boundary.

use super::*;
use crate::commander::Commander;

#[test]
fn unicode_editing_and_word_delete_keep_valid_boundaries() {
    let mut commander = Commander::default();
    commander.insert("@p17 hello 世界");
    commander.backspace(true);
    assert_eq!(commander.draft, "@p17 hello ");
    commander.cursor = 0;
    commander.delete(true);
    assert_eq!(commander.draft, "p17 hello ");
    commander.insert("λ");
    assert_eq!(commander.draft, "λp17 hello ");
}

#[test]
fn deleting_without_a_selection_clears_the_anchor() {
    let mut commander = Commander::default();
    commander.draft = "abc".into();
    commander.cursor = commander.draft.len();
    commander.selection_anchor = Some(commander.cursor);

    commander.backspace(false);
    assert_eq!(commander.draft, "ab");
    assert_eq!(commander.selection_anchor, None);
    assert!(commander.insert("d"));
    assert_eq!(commander.draft, "abd");

    commander.cursor = 0;
    commander.selection_anchor = Some(0);
    commander.delete(false);
    assert_eq!(commander.draft, "bd");
    assert_eq!(commander.selection_anchor, None);
}

#[test]
fn parser_requires_explicit_targets_and_prompt() {
    let _env = crate::persist::test_env("commander-parse");
    let (tx, _) = std::sync::mpsc::channel();
    let app = App::new(80, 24, tx).unwrap();
    assert!(app.commander_parse("hello").is_err());
    assert!(app.commander_parse("=p1 hello").is_err());
    assert!(app.commander_parse("@p1 ").is_err());
    assert!(app.commander_parse("@p1 @p1 hello").is_err());
}

#[test]
fn slash_actions_are_typed_and_unknown_names_never_reach_a_pane() {
    let _env = crate::persist::test_env("commander-slash-parser");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();

    assert!(app.commander_parse_slash_action("/task").unwrap().is_ok());
    assert!(app
        .commander_parse_slash_action("/task form")
        .unwrap()
        .is_ok());
    for action in ["/automation", "/mission", "/diff", "/files"] {
        assert!(
            app.commander_parse_slash_action(action).unwrap().is_ok(),
            "{action}"
        );
        assert!(app
            .commander_parse_slash_action(&format!("{action} extra"))
            .unwrap()
            .is_err());
    }
    assert!(app
        .commander_parse_slash_action(&format!("/split @p{} right", id.0))
        .unwrap()
        .is_ok());
    assert!(app
        .commander_parse_slash_action(&format!("/split @p{} below", id.0))
        .unwrap()
        .is_ok());
    assert!(app
        .commander_parse_slash_action(&format!("/split @p{} sideways", id.0))
        .unwrap()
        .is_err());
    assert!(app
        .commander_parse_slash_action(&format!("/read @p{}", id.0))
        .unwrap()
        .is_ok());
    assert!(app.commander_parse_slash_action("/focus").unwrap().is_err());
    assert!(app
        .commander_parse_slash_action("/task extra")
        .unwrap()
        .is_err());
    assert!(app
        .commander_parse_slash_action("/read @p1 extra")
        .unwrap()
        .is_err());
    assert!(app.commander_parse_slash_action("@p1 /status").is_none());

    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("/unknown @p{}", id.0);
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("Unknown action"));
    assert!(
        input_rx.try_recv().is_err(),
        "unknown slash text was not submitted"
    );
}

#[test]
fn commander_task_form_binds_workspace_and_reports_created_id() {
    let _env = crate::persist::test_env("commander-task-bound-workspace");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let workspace = app.workspaces[app.active_ws].id.clone();
    app.workspaces[app.active_ws].name = "commander-target".into();
    app.new_workspace();
    assert_ne!(app.workspaces[app.active_ws].id, workspace);
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/task @workspace:commander-target".into();
    app.commander_prepare();
    let form = app.orch_form.as_ref().unwrap();
    assert_eq!(form.workspace_id.as_deref(), Some(workspace.as_str()));
    assert!(form.commander_origin);
    app.orch_form.as_mut().unwrap().title = "Review changes".into();
    app.handle_orch_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.orch_form.is_none());
    assert!(app.commander.as_ref().unwrap().focused);
    assert_eq!(
        app.orch
            .tasks
            .last()
            .unwrap()
            .project
            .as_ref()
            .unwrap()
            .workspace_id,
        workspace
    );
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("Task t"));
}

#[test]
fn commander_automation_agent_target_is_exact_and_cancel_returns_focus() {
    let _env = crate::persist::test_env("commander-automation-exact-agent");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    app.open_commander();
    app.commander.as_mut().unwrap().draft = format!("/automation @p{}", pane.0);
    app.commander_prepare();
    let form = app.orch_form.as_ref().unwrap();
    assert_eq!(form.automation_target, OrchAutomationTarget::ActiveAgent);
    assert_eq!(form.active_agents.len(), 1);
    assert_eq!(form.active_agents[0].pane, pane);
    app.handle_orch_form_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.orch_form.is_none());
    assert!(app.commander.as_ref().unwrap().focused);
}

#[test]
fn guided_task_stays_in_commander_then_creates_in_exact_workspace() {
    let _env = crate::persist::test_env("commander-guided-task");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let workspace_id = app.workspaces[app.active_ws].id.clone();
    app.workspaces[app.active_ws].name = "guided-project".into();
    app.new_workspace();
    app.open_commander();
    app.commander.as_mut().unwrap().draft = "/task @workspace:guided-project guide".into();
    app.commander_prepare();
    assert!(app.orch_form.is_none());
    assert!(app.commander.as_ref().unwrap().focused);
    assert!(app.commander.as_ref().unwrap().guided_orch.is_some());
    assert!(app.orch.tasks.is_empty());
    let title_cursor = app.commander.as_ref().unwrap().cursor;
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.commander.as_ref().unwrap().cursor > title_cursor);
    app.commander_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().cursor, title_cursor);
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.commander.as_ref().unwrap().cursor > title_cursor);
    assert!(app.orch.tasks.is_empty());
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/task @workspace:guided-project\ntitle: Review tests\nstart: manual\nagent: \nmode: workspace\npaths: src/**\ndeps: \ngate: \nprompt: Check test coverage\nReport gaps".into();
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert!(app.orch_form.is_none());
    assert_eq!(app.orch.tasks.len(), 1);
    assert_eq!(
        app.orch.tasks[0].project.as_ref().unwrap().workspace_id,
        workspace_id
    );
    assert_eq!(
        app.orch.tasks[0].prompt.as_deref(),
        Some("Check test coverage\nReport gaps")
    );
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("Task t"));
    assert!(app.commander.as_ref().unwrap().guided_orch.is_none());
}

#[test]
fn guided_task_keeps_its_workspace_after_switching_workspaces() {
    let _env = crate::persist::test_env("commander-guided-workspace-switch");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let workspace_id = app.workspaces[app.active_ws].id.clone();
    app.open_commander();
    app.commander.as_mut().unwrap().draft = "/task guide".into();
    app.commander_prepare();
    app.new_workspace();
    assert_ne!(app.workspaces[app.active_ws].id, workspace_id);
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/task\ntitle: Review tests\nstart: manual\nagent: \nmode: workspace\npaths: \ndeps: \ngate: \nprompt: Check tests".into();
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert_eq!(app.orch.tasks.len(), 1);
    assert_eq!(
        app.orch.tasks[0].project.as_ref().unwrap().workspace_id,
        workspace_id
    );
}

#[test]
fn guided_automation_validates_before_creating() {
    let _env = crate::persist::test_env("commander-guided-automation");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    app.commander.as_mut().unwrap().draft = "/automation guide".into();
    app.commander_prepare();
    assert!(app.commander.as_ref().unwrap().draft.contains("schedule: "));
    app.commander_prepare();
    assert!(app.automation.automations.is_empty());
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("title"));
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/automation\ntitle: Daily review\nstart: daily\nschedule: 09:00\ntimezone: Asia/Makassar\nagent: codex\nmode: workspace\naccess: workspace\npaths: \ngate: \nprompt: Review today's changes".into();
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert_eq!(app.automation.automations.len(), 1);
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("Automation a"));
}

#[test]
fn inline_task_fields_create_without_opening_the_modal() {
    let _env = crate::persist::test_env("commander-inline-task");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.workspaces[app.active_ws].name = "inline-project".into();
    let workspace_id = app.workspaces[app.active_ws].id.clone();
    app.new_workspace();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/task @workspace:inline-project title: Review tests  start: manual  prompt: Check the start: behavior".into();
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert!(app.orch_form.is_none());
    assert_eq!(app.orch.tasks.len(), 1);
    assert_eq!(app.orch.tasks[0].title, "Review tests");
    assert_eq!(
        app.orch.tasks[0].project.as_ref().unwrap().workspace_id,
        workspace_id
    );
    assert_eq!(
        app.orch.tasks[0].prompt.as_deref(),
        Some("Check the start: behavior")
    );
}

#[test]
fn inline_automation_tab_cycles_start_and_timezone_then_creates() {
    let _env = crate::persist::test_env("commander-inline-automation");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/automation title: Daily review  start:  schedule: 09:00  timezone:  agent: codex  mode: workspace  access: workspace  prompt: Review changes".into();
    commander.cursor = commander.draft.find("start:").unwrap() + "start:".len();
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .contains("start: once"));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .contains("start: hourly"));
    app.commander_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .contains("start: once"));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .contains("start: daily"));
    let commander = app.commander.as_mut().unwrap();
    commander.cursor = commander.draft.find("timezone:").unwrap() + "timezone:".len();
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let draft = &app.commander.as_ref().unwrap().draft;
    let timezone = draft
        .split("timezone:")
        .nth(1)
        .unwrap()
        .split("agent:")
        .next()
        .unwrap()
        .trim();
    assert!(!timezone.is_empty());
    assert!(jiff::tz::db().get(timezone).is_ok());
    app.commander_prepare();
    assert!(app.orch_form.is_none());
    assert_eq!(
        app.automation.automations.len(),
        1,
        "Commander receipt: {:?}",
        app.commander.as_ref().unwrap().receipt
    );
    assert_eq!(app.automation.automations[0].name, "Daily review");
}

#[test]
fn inline_agent_automation_stays_in_commander_while_bare_target_opens_modal() {
    let _env = crate::persist::test_env("commander-inline-agent-automation");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    app.open_commander();
    app.commander.as_mut().unwrap().draft = format!("/automation @p{}", pane.0);
    app.commander_prepare();
    assert_eq!(
        app.orch_form.as_ref().unwrap().automation_target,
        OrchAutomationTarget::ActiveAgent
    );
    app.close_orch_form();

    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("/automation @p{} title: hello  start:", pane.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .ends_with("start: once"));
    app.commander_prepare();
    assert!(app.orch_form.is_none());
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("schedule"));
}

#[test]
fn tab_builds_active_agent_automation_fields_in_order() {
    let _env = crate::persist::test_env("commander-inline-agent-fields");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    app.open_commander();
    app.commander.as_mut().unwrap().draft = format!("/automation @p{}", pane.0);
    app.commander.as_mut().unwrap().cursor = app.commander.as_ref().unwrap().draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.commander.as_ref().unwrap().draft.ends_with("title: "));
    app.commander.as_mut().unwrap().insert("hello");
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.commander.as_ref().unwrap().draft.ends_with("start: "));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .ends_with("start: once"));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .ends_with("schedule: "));
    app.commander.as_mut().unwrap().insert("2026-12-01 09:00");
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .ends_with("timezone: "));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let timezone = app.commander.as_ref().unwrap().draft.clone();
    assert!(timezone.contains("timezone: "));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let draft = &app.commander.as_ref().unwrap().draft;
    assert!(draft.ends_with("prompt: "));
    assert!(!draft.contains("agent:"));
    assert!(!draft.contains("access:"));
    assert!(app.orch_form.is_none());
}

#[test]
fn tab_completes_slash_then_builds_task_fields_with_selected_agent() {
    let _env = crate::persist::test_env("commander-inline-task-fields");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    app.open_commander();
    app.commander.as_mut().unwrap().draft = "/ta".into();
    app.commander.as_mut().unwrap().cursor = 3;
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/task");
    app.commander
        .as_mut()
        .unwrap()
        .insert(&format!(" @p{}", pane.0));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.commander.as_ref().unwrap().draft.ends_with("title: "));
    app.commander.as_mut().unwrap().insert("review");
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.commander.as_ref().unwrap().draft.ends_with("start: "));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .ends_with("start: manual"));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .ends_with("agent: codex"));
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.commander.as_ref().unwrap().draft.ends_with("mode: "));
}

#[test]
fn working_agent_requires_second_enter_unless_auto_send_and_blocked_never_sends() {
    let _env = crate::persist::test_env("commander-working-confirmation");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.config.commander_working_policy = crate::config::CommanderWorkingPolicy::Ask;
    let pane = app.layout().focus;
    let generation = app.panes[&pane].engine.lock().unwrap().output_generation();
    let status = app.status.get_mut(&pane).unwrap();
    status.agent = "claude".into();
    status.state = crate::ui::theme::State::Working;
    status.prompt_evidence = crate::detect::PromptEvidence::Ready;
    status.last_detect_generation = Some(generation);
    status.force_detect = false;
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    let draft = format!("@p{} continue", pane.0);
    app.commander.as_mut().unwrap().draft = draft.clone();
    app.commander_prepare();
    assert_eq!(
        app.commander
            .as_ref()
            .unwrap()
            .pending_working_confirmation
            .as_deref(),
        Some(draft.as_str())
    );
    assert!(input_rx.try_recv().is_err());
    app.status.get_mut(&pane).unwrap().state = crate::ui::theme::State::Blocked;
    app.commander_prepare();
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("blocked"));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .pending_working_confirmation
        .is_none());
    assert!(input_rx.try_recv().is_err());
    app.status.get_mut(&pane).unwrap().state = crate::ui::theme::State::Working;
    app.commander_prepare();
    assert!(input_rx.try_recv().is_err());
    app.commander_prepare();
    assert!(input_rx.try_recv().is_ok());

    app.commander.as_mut().unwrap().insert("now");
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .pending_working_confirmation
        .is_none());
    app.config.commander_working_policy = crate::config::CommanderWorkingPolicy::AutoSend;
    app.commander_prepare();
    assert!(input_rx.try_recv().is_ok());

    app.status.get_mut(&pane).unwrap().state = crate::ui::theme::State::Blocked;
    app.commander.as_mut().unwrap().draft = draft;
    app.commander_prepare();
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("blocked"));
    assert!(input_rx.try_recv().is_err());
}

#[test]
fn slash_key_and_tab_complete_the_action_before_a_target() {
    let _env = crate::persist::test_env("commander-slash-complete");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    app.commander_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/");
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/focus");
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/read");
    app.commander_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/focus");
    app.commander_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/focus ");
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .starts_with("/focus @"));
}

#[test]
fn slash_enter_accepts_a_suggestion_before_dispatching() {
    let _env = crate::persist::test_env("commander-slash-enter");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    app.commander_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    app.commander_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    app.commander_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/automation");
    assert!(app.orch_form.is_none());
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.orch_form.as_ref().unwrap().kind,
        OrchFormKind::Automation
    );
    assert!(app.commander.as_ref().unwrap().guided_orch.is_none());
}

#[test]
fn slash_picker_arrows_pages_and_enter_select_without_editing_the_draft() {
    let _env = crate::persist::test_env("commander-slash-browse");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    app.commander_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    for _ in 0..8 {
        app.commander_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    let commander = app.commander.as_ref().unwrap();
    assert_eq!(commander.draft, "/");
    assert_eq!(commander.slash_selection, Some(8));
    app.commander_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().slash_selection, Some(7));
    app.commander_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().slash_selection, Some(2));
    app.commander_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().slash_selection, Some(7));
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/diff");
    assert_eq!(app.commander.as_ref().unwrap().slash_selection, None);
    assert!(!app.files_focused, "selection is not action execution");
}

#[test]
fn slash_picker_mouse_wheel_and_row_click_stay_out_of_underlying_panes() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-slash-mouse");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.open_commander();
    app.commander_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    app.commander_area = Some(Rect::new(3, 20, 74, 4));
    app.last_pane_area = Rect::new(3, 11, 74, 9);
    let (popup, _, _) = app.commander_slash_popup().unwrap();
    let mouse = |kind, row| {
        crate::event::AppEvent::Mouse(MouseEvent {
            kind,
            column: popup.x + 4,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    assert!(app.handle_event(mouse(MouseEventKind::ScrollDown, popup.y + 2)));
    assert_eq!(app.commander.as_ref().unwrap().slash_selection, Some(1));
    assert!(app.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), popup.y + 4)));
    assert_eq!(app.commander.as_ref().unwrap().slash_selection, Some(3));
    assert!(app.commander.as_ref().unwrap().focused);
    assert_eq!(app.layout().focus, pane);
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "/split");
}

#[test]
fn slash_forms_and_docks_reuse_existing_controls() {
    let _env = crate::persist::test_env("commander-slash-controls");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    fn invoke(app: &mut App, action: &str) {
        let commander = app.commander.as_mut().unwrap();
        commander.draft = action.into();
        commander.cursor = commander.draft.len();
        commander.focused = true;
        app.commander_prepare();
    }
    invoke(&mut app, "/automation form");
    assert_eq!(
        app.orch_form.as_ref().unwrap().kind,
        OrchFormKind::Automation
    );
    app.orch_form = None;
    invoke(&mut app, "/mission");
    assert!(app.active_is_mission());
    assert!(!app.commander.as_ref().unwrap().focused);
    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.active_is_mission());
    assert!(app.commander.as_ref().unwrap().focused);
    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.active_is_mission());
    assert!(app.commander.is_none());
    app.open_commander();
    invoke(&mut app, "/diff");
    assert_eq!(app.files_mode, crate::diff::FilesMode::Diff);
    assert!(app.files_focused);
    invoke(&mut app, "/files");
    assert_eq!(app.files_mode, crate::diff::FilesMode::Files);
    assert!(app.files_focused);
}

#[test]
fn slash_split_requires_one_exact_live_pane_and_direction() {
    let _env = crate::persist::test_env("commander-slash-split");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    let before = app.layout().len();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("/split @p{} right", id.0);
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert_eq!(app.layout().len(), before + 1);
    assert!(!app.commander.as_ref().unwrap().focused);
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("split right"));
}

#[test]
fn slash_focus_uses_an_exact_pane_or_named_tab_without_sending_input() {
    let _env = crate::persist::test_env("commander-slash-focus");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    let second = app.split_pane(first, Axis::Col, true).unwrap();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&second)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.focus_pane_global(first);
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("/focus @p{}", second.0);
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert_eq!(app.layout().focus, second);
    assert!(!app.commander.as_ref().unwrap().focused);
    assert!(input_rx.try_recv().is_err());

    app.workspaces[app.active_ws].tabs[0].name = Some("review".into());
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/focus @tab:review".into();
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("Focused"));
    app.workspaces[app.active_ws].name = "review work".into();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/focus @workspace:review%20work".into();
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("Focused"));
    assert!(app
        .commander_parse_slash_action("/focus @session:not-this-one/tab:review")
        .unwrap()
        .is_err());
}

#[test]
fn slash_read_shows_bounded_output_and_task_opens_existing_form() {
    let _env = crate::persist::test_env("commander-slash-read-task");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    app.panes[&id]
        .engine
        .lock()
        .unwrap()
        .advance(b"commander read marker\r\n");
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("/read @p{}", id.0);
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    let commander = app.commander.as_ref().unwrap();
    assert!(commander
        .read_output
        .as_ref()
        .unwrap()
        .iter()
        .any(|line| line.contains("commander read marker")));
    assert!(app.commander_height >= 12);
    app.commander_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert!(app.commander.as_ref().unwrap().read_scroll > 0);

    let commander = app.commander.as_mut().unwrap();
    commander.draft = "/task form".into();
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert_eq!(app.orch_form.as_ref().unwrap().kind, OrchFormKind::Task);
    assert!(app.commander.as_ref().unwrap().read_output.is_none());
    assert!(!app.commander.as_ref().unwrap().focused);
}

#[test]
fn slash_fork_rejects_a_shell_without_creating_a_pane() {
    let _env = crate::persist::test_env("commander-slash-fork-shell");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    let before = app.panes.len();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("/fork @p{}", id.0);
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert_eq!(app.panes.len(), before);
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("does not support"));
}

#[test]
fn slash_fork_reuses_the_native_agent_fork_handler() {
    let _env = crate::persist::test_env("commander-slash-fork-agent");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let source = app.layout().focus;
    let before = app.layout().len();
    let status = app.status.get_mut(&source).unwrap();
    status.agent = "claude".into();
    status.agent_session = Some(AgentSession {
        agent: "claude".into(),
        session_id: "commander-fork-source".into(),
    });
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("/fork @p{}", source.0);
    commander.cursor = commander.draft.len();
    app.commander_prepare();
    assert_eq!(app.layout().len(), before + 1);
    assert_ne!(app.layout().focus, source);
    assert_eq!(app.status[&app.layout().focus].agent, "claude");
    assert!(!app.commander.as_ref().unwrap().focused);
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_deref()
        .unwrap()
        .contains("Forked"));
}

#[test]
fn literal_arguments_and_escaped_mentions_remain_in_the_prompt() {
    let _env = crate::persist::test_env("commander-literal-arguments");
    let (tx, _) = std::sync::mpsc::channel();
    let app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    let draft = format!("@p{} npm install @types/node =value =p999999 \\@p88", id.0);
    let plan = app.commander_parse(&draft).unwrap();
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.targets[0].pane, id);
    assert_eq!(
        plan.targets[0].prompt,
        "npm install @types/node =value =p999999 @p88"
    );
    let inline = app
        .commander_parse(&format!("echo hello @p{}", id.0))
        .unwrap();
    assert_eq!(inline.targets[0].pane, id);
    assert_eq!(inline.targets[0].prompt, "echo hello");
    let package_first = app
        .commander_parse(&format!("@types/node @p{} install", id.0))
        .unwrap();
    assert_eq!(package_first.targets[0].prompt, "@types/node install");
    assert!(app
        .commander_parse(&format!("@p{} echo hi @p999999", id.0))
        .is_err());
}

#[test]
fn inline_mentions_select_multiple_exact_panes_without_entering_the_message() {
    let _env = crate::persist::test_env("commander-mentions");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.new_tab();
    let second = app.layout().focus;
    app.status.get_mut(&first).unwrap().agent = "zsh".into();
    app.status.get_mut(&second).unwrap().agent = "zsh".into();
    let (first_tx, first_rx) = std::sync::mpsc::channel();
    let (second_tx, second_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&first)
        .unwrap()
        .replace_input_sender_for_test(first_tx);
    app.panes
        .get_mut(&second)
        .unwrap()
        .replace_input_sender_for_test(second_tx);
    let draft = format!("@p{} echo hello @p{} world", first.0, second.0);
    let parsed = app.commander_parse(&draft).unwrap();
    assert_eq!(parsed.targets.len(), 2);
    assert_eq!(parsed.targets[0].pane, first);
    assert_eq!(parsed.targets[1].pane, second);
    assert_eq!(parsed.targets[0].prompt, "echo hello");
    assert_eq!(parsed.targets[1].prompt, "world");

    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("@p{} echo hello @p{} ls", first.0, second.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    for (rx, expected) in [(first_rx, "echo hello"), (second_rx, "ls")] {
        let crate::terminal::pty::InputAction::Submit { paste, .. } = rx.try_recv().unwrap() else {
            panic!("each selected pane gets one atomic submit");
        };
        assert_eq!(String::from_utf8_lossy(&paste), expected);
        assert!(rx.try_recv().is_err());
    }
    assert!(app.commander.as_ref().unwrap().draft.is_empty());
    assert_eq!(app.commander.as_ref().unwrap().delivery_results.len(), 2);
}

#[test]
fn adjacent_mentions_share_only_their_following_segment() {
    let _env = crate::persist::test_env("commander-target-groups");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.new_tab();
    let second = app.layout().focus;
    app.new_tab();
    let third = app.layout().focus;
    let draft = format!("@p{} @p{} review this @p{} ls", first.0, second.0, third.0);
    let plan = app.commander_parse(&draft).unwrap();
    assert_eq!(plan.targets.len(), 3);
    assert_eq!(plan.targets[0].prompt, "review this");
    assert_eq!(plan.targets[1].prompt, "review this");
    assert_eq!(plan.targets[2].prompt, "ls");
    let inline = app
        .commander_parse(&format!("please @p{} @p{} review", first.0, second.0))
        .unwrap();
    assert_eq!(inline.targets[0].prompt, "please review");
    assert_eq!(inline.targets[1].prompt, "please review");
    assert!(app
        .commander_parse(&format!("@p{} review this @p{}", first.0, second.0))
        .is_err());
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&first)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("@p{} review this @p{}", first.0, second.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        input_rx.try_recv().is_err(),
        "an incomplete segment sends nothing"
    );
}

#[test]
fn agent_prompt_and_shell_command_do_not_leak_across_segments() {
    let _env = crate::persist::test_env("commander-agent-shell-segments");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let agent = app.layout().focus;
    app.new_tab();
    let shell = app.layout().focus;
    app.status.get_mut(&agent).unwrap().agent = "claude".into();
    let (agent_tx, agent_rx) = std::sync::mpsc::channel();
    let (shell_tx, shell_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&agent)
        .unwrap()
        .replace_input_sender_for_test(agent_tx);
    app.panes
        .get_mut(&shell)
        .unwrap()
        .replace_input_sender_for_test(shell_tx);

    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!(
        "@p{} Okay you got it, this feature is cool. @p{} ls",
        agent.0, shell.0
    );
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let crate::terminal::pty::InputAction::Submit {
        paste: agent_paste, ..
    } = agent_rx.try_recv().unwrap()
    else {
        panic!("agent receives one prompt");
    };
    let crate::terminal::pty::InputAction::Submit {
        paste: shell_paste, ..
    } = shell_rx.try_recv().unwrap()
    else {
        panic!("shell receives one command");
    };
    let agent_text = String::from_utf8_lossy(&agent_paste);
    let shell_text = String::from_utf8_lossy(&shell_paste);
    assert!(agent_text.contains("Okay you got it, this feature is cool."));
    assert!(!agent_text.contains(" ls"));
    assert_eq!(shell_text, "ls");
    assert!(agent_rx.try_recv().is_err());
    assert!(shell_rx.try_recv().is_err());
}

#[test]
fn partial_delivery_keeps_receipts_but_clears_mixed_draft() {
    let _env = crate::persist::test_env("commander-partial-segments");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.new_tab();
    let second = app.layout().focus;
    let draft = format!("@p{} echo ready @p{} ls", first.0, second.0);
    let plan = app.commander_parse(&draft).unwrap();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&first)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.status.get_mut(&second).unwrap().agent = "claude".into();
    app.open_commander();
    app.commander.as_mut().unwrap().draft = draft;
    app.commander_dispatch(plan);
    assert!(input_rx.try_recv().is_ok());
    let commander = app.commander.as_ref().unwrap();
    assert!(commander.draft.is_empty());
    assert_eq!(
        commander.delivery_results[0],
        format!("p{}: queued", first.0)
    );
    assert_eq!(
        commander.delivery_results[1],
        format!("p{}: no longer available", second.0)
    );
}

#[test]
fn named_scope_paths_resolve_one_pane_and_tab_completes_them() {
    let _env = crate::persist::test_env("commander-named-scopes");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.new_tab();
    let second = app.layout().focus;
    app.workspaces[0].name = "my project".into();
    app.workspaces[0].tabs[1].name = Some("review work".into());
    app.set_agent_name(second, Some("reviewer"));

    let path = "@workspace:my%20project/tab:review%20work/pane:reviewer";
    let plan = app.commander_parse(&format!("{path} ls")).unwrap();
    assert_eq!(plan.targets.len(), 1);
    assert_eq!(plan.targets[0].pane, second);
    assert_eq!(plan.targets[0].prompt, "ls");
    let current_session = crate::session::display_name();
    let current_path = format!("@session:{current_session}/pane:reviewer ls");
    assert_eq!(
        app.commander_parse(&current_path).unwrap().targets[0].pane,
        second
    );
    assert!(app
        .commander_parse(&format!(
            "@workspace:my%20project/tab:review%20work/pane:p{} ls",
            first.0
        ))
        .is_err());
    assert!(app.commander_parse("@tab:review%20work ls").is_err());
    assert!(app
        .commander_parse("@session:other-session/pane:reviewer ls")
        .is_err());

    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "@tab:review%20work".into();
    commander.cursor = commander.draft.len();
    app.refresh_commander_preview();
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        "@tab:review%20work/pane:reviewer"
    );

    let commander = app.commander.as_mut().unwrap();
    commander.draft = "@tab:".into();
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(app.commander.as_ref().unwrap().draft, "@tab:review%20work");
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        "@tab:review%20work/pane:reviewer"
    );
}

#[test]
fn tab_completes_the_typed_at_in_place_with_a_pane_name() {
    let _env = crate::persist::test_env("commander-inline-at-completion");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.new_tab();
    let second = app.layout().focus;
    app.set_agent_name(first, Some("build"));
    app.set_agent_name(second, Some("shell_two"));
    app.open_commander();

    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("@p{} hello @", first.0);
    commander.cursor = commander.draft.len();
    app.refresh_commander_preview();
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("@p{} hello @shell_two", first.0)
    );
    app.commander_paste(" world");
    let plan = app
        .commander_parse(&app.commander.as_ref().unwrap().draft)
        .unwrap();
    assert_eq!(plan.targets.len(), 2);
    assert_eq!(plan.targets[1].pane, second);
    assert_eq!(plan.targets[0].prompt, "hello");
    assert_eq!(plan.targets[1].prompt, "world");

    let commander = app.commander.as_mut().unwrap();
    commander.draft = "@".into();
    commander.cursor = 1;
    app.refresh_commander_preview();
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "@build");
}

#[test]
fn split_actions_target_exact_panes_and_cannot_repeat_on_enter() {
    let _env = crate::persist::test_env("commander-split-actions");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.new_tab();
    let second = app.layout().focus;
    app.workspaces[0].tabs[1].name = Some("shell".into());
    app.set_agent_name(first, Some("build"));
    app.open_commander();

    let commander = app.commander.as_mut().unwrap();
    commander.draft = "@build:sv".into();
    commander.cursor = commander.draft.len();
    app.refresh_commander_preview();
    assert_eq!(app.commander.as_ref().unwrap().preview, vec![first]);
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let right = app.layout().focus;
    assert_eq!(app.active_ws, 0);
    assert_eq!(app.workspaces[0].active_tab, 0);
    assert_ne!(right, first);
    assert_eq!(app.pane_location(right), Some((0, 0)));
    assert!(matches!(
        app.layout().to_tree(),
        crate::layout::LayoutTree::Split { axis: 0, .. }
    ));
    assert!(!app.commander.as_ref().unwrap().focused);
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("@p{} ", right.0)
    );
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_ref()
        .unwrap()
        .contains("split right"));
    app.commander.as_mut().unwrap().focused = true;
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.workspaces[0].tabs[0].layout.len(), 2);

    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("@tab:shell/pane:p{}:sh", second.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let below = app.layout().focus;
    assert_eq!(app.workspaces[0].active_tab, 1);
    assert_eq!(app.pane_location(below), Some((0, 1)));
    assert!(matches!(
        app.layout().to_tree(),
        crate::layout::LayoutTree::Split { axis: 1, .. }
    ));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_ref()
        .unwrap()
        .contains("split below"));
}

#[test]
fn split_action_rejects_extra_text_before_mutating_layout() {
    let _env = crate::persist::test_env("commander-split-extra-text");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("@p{}:sv hello", pane.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.layout().len(), 1);
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .receipt
        .as_ref()
        .unwrap()
        .contains("no message"));
}

#[test]
fn prefix_enter_toggles_the_composer_without_discarding_it_on_send() {
    let _env = crate::persist::test_env("commander-toggle");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    app.status.get_mut(&id).unwrap().agent = "zsh".into();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("@p{} ls", id.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(input_rx.try_recv().is_ok());
    assert!(app.commander.is_some());
    let draft = app.commander.as_ref().unwrap().draft.clone();
    app.handle_event(AppEvent::Mouse(ratatui::crossterm::event::MouseEvent {
        kind: ratatui::crossterm::event::MouseEventKind::Down(
            ratatui::crossterm::event::MouseButton::Left,
        ),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);

    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.commander.as_ref().unwrap().focused);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.commander.is_none());
    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.commander.as_ref().unwrap().focused);
    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(app.commander.is_none());
}

#[test]
fn editing_chords_selection_and_clipboard_paste_stay_private() {
    let _env = crate::persist::test_env("commander-editing");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "hello 世界".into();
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(app.commander.as_ref().unwrap().draft, "hello ");
    app.commander_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(app.commander.as_ref().unwrap().draft.is_empty());
    app.commander_paste("first");
    app.commander_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SUPER));
    app.commander_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER));
    assert_eq!(
        app.commander.as_ref().unwrap().pending_clipboard.as_deref(),
        Some("first")
    );
    assert!(app.pending_clipboard.is_none());
    app.commander_paste("second");
    assert_eq!(app.commander.as_ref().unwrap().draft, "second");
    app.commander_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER));
    assert!(app.commander.as_ref().unwrap().draft.is_empty());
    app.commander_paste("first\nsecond/path");
    app.commander_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(app.commander.as_ref().unwrap().draft, "first\nsecond/");
    app.commander_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.commander.as_ref().unwrap().draft, "first\n");
    app.commander_key(KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert!(app.commander.as_ref().unwrap().draft.is_empty());
}

#[test]
fn shift_enter_opens_a_real_line_and_arrows_edit_that_line() {
    let _env = crate::persist::test_env("commander-multiline-editing");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let initial = app.commander.as_ref().unwrap().draft.clone();
    app.commander_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    app.commander_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("{initial}a\nb")
    );
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "alpha\nbeta".into();
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    app.commander_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "alph!a\nbeta");
    app.commander_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.commander_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().draft, "alph!a\nbeta!");
    // ESC-CR is the fallback where a terminal cannot report Shift+Enter.
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
    assert_eq!(app.commander.as_ref().unwrap().draft, "alph!a\nbeta!\n");
}

#[test]
fn clipboard_image_is_discarded_when_draft_is_abandoned_or_edited_away() {
    let _env = crate::persist::test_env("commander-image-paste");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let png =
        crate::clipboard_image::encode_rgba_png(1, 1, |_, _| [1, 2, 3, 255]).expect("valid png");
    let staged = crate::clipboard_image::stage_png(&png).expect("staged image");
    assert!(app.handle_event(AppEvent::PasteImage(staged.clone())));
    assert!(app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .contains(&staged.to_string_lossy().to_string()));
    assert!(staged.exists());
    app.open_commander();
    assert!(!staged.exists());

    app.open_commander();
    let staged = crate::clipboard_image::stage_png(&png).expect("second staged image");
    assert!(app.handle_event(AppEvent::PasteImage(staged.clone())));
    app.commander_key(KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert!(!staged.exists());
}

#[test]
fn clipboard_image_marker_moves_and_deletes_as_one_token() {
    let _env = crate::persist::test_env("commander-image-atomic-edit");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let png =
        crate::clipboard_image::encode_rgba_png(1, 1, |_, _| [1, 2, 3, 255]).expect("valid png");
    let staged = crate::clipboard_image::stage_png(&png).expect("staged image");
    assert!(app.handle_event(AppEvent::PasteImage(staged.clone())));
    let image_start = app
        .commander
        .as_ref()
        .unwrap()
        .draft
        .find(&staged.to_string_lossy().to_string())
        .unwrap();
    app.commander_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(app.commander.as_ref().unwrap().cursor, image_start);
    app.commander_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(
        app.commander.as_ref().unwrap().cursor,
        app.commander.as_ref().unwrap().draft.len()
    );
    app.commander_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert!(!staged.exists());
    assert_eq!(app.commander.as_ref().unwrap().draft.len(), image_start);
}

#[test]
fn delivered_image_path_survives_composer_close() {
    let _env = crate::persist::test_env("commander-image-delivery");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    let png =
        crate::clipboard_image::encode_rgba_png(1, 1, |_, _| [1, 2, 3, 255]).expect("valid png");
    let staged = crate::clipboard_image::stage_png(&png).expect("staged image");
    assert!(app.handle_event(AppEvent::PasteImage(staged.clone())));
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(input_rx.try_recv().is_ok());
    app.open_commander();
    assert!(staged.exists());
    crate::clipboard_image::discard_staged_png(&staged);
}

#[test]
fn full_draft_tab_does_not_move_cursor_after_rejected_insert() {
    let _env = crate::persist::test_env("commander-full-draft-tab");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = "λ".repeat(16_384);
    commander.cursor = 0;
    app.commander_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let commander = app.commander.as_ref().unwrap();
    assert_eq!(commander.cursor, 0);
    assert_eq!(commander.draft.chars().count(), 16_384);
}

#[test]
fn help_overlay_owns_keys_mouse_and_paste_above_the_composer() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-help-priority");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.open_commander();
    app.commander_area = Some(Rect::new(0, 20, 80, 4));
    let draft = app.commander.as_ref().unwrap().draft.clone();
    app.help_open = true;
    assert!(app.handle_event(AppEvent::Paste("hidden input".into())));
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
    assert!(app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    ))));
    assert!(!app.help_open);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);

    app.help_open = true;
    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: 21,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(!app.help_open);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
}

#[test]
fn exact_shell_target_queues_one_atomic_command() {
    let _env = crate::persist::test_env("commander-shell-submit");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    app.status.get_mut(&id).unwrap().agent = "zsh".into();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);

    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("@p{} ls", id.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let crate::terminal::pty::InputAction::Submit { paste, .. } = input_rx.try_recv().unwrap()
    else {
        panic!("shell command must be one atomic paste-and-Enter action");
    };
    assert!(String::from_utf8_lossy(&paste).contains("ls"));
    assert!(input_rx.try_recv().is_err());
    assert_eq!(
        app.commander.as_ref().unwrap().delivery_results,
        vec![format!("p{}: queued", id.0)]
    );
}

#[test]
fn shell_target_rejects_multiline_and_agent_identity_drift() {
    let _env = crate::persist::test_env("commander-shell-safety");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let id = app.layout().focus;
    app.status.get_mut(&id).unwrap().agent = "zsh".into();
    assert!(app
        .commander_parse(&format!("@p{} echo one\necho two", id.0))
        .is_err());

    let plan = app.commander_parse(&format!("@p{} ls", id.0)).unwrap();
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&id)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    app.status.get_mut(&id).unwrap().agent = "codex".into();
    app.commander_dispatch(plan);
    assert!(input_rx.try_recv().is_err());
    assert_eq!(
        app.commander.as_ref().unwrap().delivery_results,
        vec![format!("p{}: no longer available", id.0)]
    );
}

#[test]
fn multiple_mentions_send_directly_without_changing_layout_or_focus() {
    let _env = crate::persist::test_env("commander-direct-multiple");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let first = app.layout().focus;
    app.status.get_mut(&first).unwrap().agent = "zsh".into();
    app.new_tab();
    let second = app.layout().focus;
    app.status.get_mut(&second).unwrap().agent = "zsh".into();
    let (first_tx, first_rx) = std::sync::mpsc::channel();
    let (second_tx, second_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&first)
        .unwrap()
        .replace_input_sender_for_test(first_tx);
    app.panes
        .get_mut(&second)
        .unwrap()
        .replace_input_sender_for_test(second_tx);
    let pane_count = app.panes.len();
    let tab_count = app.ws().tabs.len();
    app.open_commander();
    let commander = app.commander.as_mut().unwrap();
    commander.draft = format!("@p{} @p{} review this", first.0, second.0);
    commander.cursor = commander.draft.len();
    app.commander_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    for rx in [first_rx, second_rx] {
        let crate::terminal::pty::InputAction::Submit { paste, .. } = rx.try_recv().unwrap() else {
            panic!("each @ target must receive one submit on the first Enter");
        };
        assert!(String::from_utf8_lossy(&paste).contains("review this"));
        assert!(rx.try_recv().is_err());
    }
    assert_eq!(app.layout().focus, second);
    assert_eq!(app.panes.len(), pane_count);
    assert_eq!(app.ws().tabs.len(), tab_count);
    assert!(app.commander.is_some());
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("@p{} @p{} ", first.0, second.0)
    );
}

#[test]
fn paste_and_navigation_stay_in_composer() {
    let _env = crate::persist::test_env("commander-input");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let focused = app.layout().focus;
    app.open_commander();
    assert!(app.handle_event(AppEvent::Paste("hello 世界".into())));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("@p{} hello 世界", focused.0)
    );
    assert_eq!(app.layout().focus, focused);
    assert!(app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::CONTROL
    ))));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("@p{} hello ", focused.0)
    );
}

#[test]
fn prefix_navigation_keeps_the_strip_visible_and_preserves_targets() {
    let _env = crate::persist::test_env("commander-navigation");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.new_tab();
    app.switch_tab(0);
    app.new_workspace();
    app.cycle_workspace(-1);
    assert_eq!((app.active_ws, app.ws().active_tab), (0, 0));
    app.open_commander();
    let draft = app.commander.as_ref().unwrap().draft.clone();

    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::NONE,
    )));
    assert_eq!((app.active_ws, app.ws().active_tab), (0, 1));
    assert!(app.commander.is_some());
    assert!(!app.commander.as_ref().unwrap().focused);

    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.active_ws, 1);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
}

#[test]
fn clicking_the_strip_restores_editing_without_blocking_pane_paste() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-focus");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    let (input_tx, input_rx) = std::sync::mpsc::channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(input_tx);
    app.open_commander();
    app.commander_area = Some(Rect::new(0, 20, 80, 4));
    let draft = app.commander.as_ref().unwrap().draft.clone();

    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert!(!app.commander.as_ref().unwrap().focused);
    assert!(app.commander.is_some());
    app.handle_event(AppEvent::Paste("pane paste".into()));
    assert!(input_rx.try_recv().is_ok());
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);

    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: 21,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(app.commander.as_ref().unwrap().focused);
    app.handle_event(AppEvent::Paste(" composer".into()));
    assert_eq!(
        app.commander.as_ref().unwrap().draft,
        format!("{draft} composer")
    );
    assert!(input_rx.try_recv().is_err());

    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 40,
        row: 10,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(!app.commander.as_ref().unwrap().focused);
    assert!(app.commander.is_some());
}

#[test]
fn clicking_the_strip_in_prefix_mode_resumes_editing() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-prefix-click");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.new_tab();
    app.switch_tab(0);
    app.open_commander();
    app.commander_area = Some(Rect::new(0, 20, 80, 4));
    let draft = app.commander.as_ref().unwrap().draft.clone();

    app.handle_event(AppEvent::Key(app.prefix.key_event()));
    assert_eq!(app.mode, Mode::Prefix);
    assert!(app.commander.as_ref().unwrap().focused);
    assert!(!app.commander_accepts_input());

    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row: 21,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.commander.as_ref().unwrap().focused);

    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::NONE,
    )));
    assert_eq!(app.commander.as_ref().unwrap().draft, format!("{draft}n"));
    assert_eq!(app.ws().active_tab, 0);
}

#[test]
fn clicking_another_tab_works_while_the_strip_stays_open() {
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let _env = crate::persist::test_env("commander-tab-click");
    let (tx, _) = std::sync::mpsc::channel();
    let mut app = App::new(80, 24, tx).unwrap();
    app.new_tab();
    app.switch_tab(0);
    app.open_commander();
    let draft = app.commander.as_ref().unwrap().draft.clone();
    app.commander_area = Some(Rect::new(0, 20, 80, 4));
    app.tab_rects = vec![(1, Rect::new(40, 3, 10, 1))];

    app.handle_event(AppEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 41,
        row: 3,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(app.ws().active_tab, 1);
    assert!(!app.commander.as_ref().unwrap().focused);
    assert_eq!(app.commander.as_ref().unwrap().draft, draft);
}
