use super::*;
use crate::terminal::pty::InputAction;
use std::sync::mpsc::{channel, Receiver};

fn fixture() -> (App, PaneId, Receiver<InputAction>) {
    let (tx, _) = channel();
    let mut app = App::new(80, 24, tx).unwrap();
    let pane = app.layout().focus;
    app.status.get_mut(&pane).unwrap().agent = "codex".into();
    app.status.get_mut(&pane).unwrap().state = State::Idle;
    let (tx, rx) = channel();
    app.panes
        .get_mut(&pane)
        .unwrap()
        .isolate_engine_for_test(tx.clone());
    app.panes
        .get_mut(&pane)
        .unwrap()
        .replace_input_sender_for_test(tx);
    screen(&app, pane, "\x1b[2J\x1b[H\r\n› ");
    (app, pane, rx)
}

fn screen(app: &App, pane: PaneId, text: &str) {
    let target = &app.panes[&pane];
    let mut engine = target.engine.lock().unwrap();
    engine.advance(text.as_bytes());
    target
        .content_revision_handle()
        .fetch_add(1, Ordering::Release);
}

fn start(app: &mut App, pane: PaneId, mut params: Value) -> (Receiver<String>, Arc<AtomicBool>) {
    params["target"] = json!(pane.0.to_string());
    let (tx, rx) = channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    app.start_agent_prompt("echo-test".into(), params, tx, cancelled.clone());
    (rx, cancelled)
}

fn response(rx: &Receiver<String>) -> Value {
    serde_json::from_str(&rx.try_recv().expect("bounded workflow must respond")).unwrap()
}

fn bytes(rx: &Receiver<InputAction>, expected: &[u8]) {
    match rx.try_recv().expect("one input action") {
        InputAction::Bytes(actual) => assert_eq!(actual, expected),
        InputAction::Submit { .. } => panic!("Enter must not accompany the paste"),
    }
    assert!(rx.try_recv().is_err(), "no extra input action");
}

fn timeout(app: &mut App) {
    app.tick_agent_workflows(Instant::now() + Duration::from_secs(3));
}

#[test]
fn observed_prompt_login_progress_is_not_prompt_submission() {
    let _env = crate::persist::test_env("echo-login");
    let (mut app, pane, input) = fixture();
    screen(
        &app,
        pane,
        "\x1b[2J\x1b[HWelcome to Codex\r\n1. Sign in with ChatGPT\r\n2. API key",
    );
    let (rx, _) = start(&mut app, pane, json!({"text":"unique login prompt"}));
    bytes(&input, b"unique login prompt");
    app.status.get_mut(&pane).unwrap().state = State::Working;
    screen(&app, pane, "\r\nOpening browser...");
    timeout(&mut app);
    let value = response(&rx);
    assert_eq!(value["error"]["code"], "input_not_echoed");
    assert_eq!(value["error"]["data"]["submitted"], false);
    assert_eq!(app.status[&pane].state, State::Working);
    assert!(input.try_recv().is_err());
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn confirmed_prompt_echoes_first_line_then_sends_exactly_one_enter() {
    let _env = crate::persist::test_env("echo-first-line");
    for bracketed in [false, true] {
        for text in [
            "unique prompt",
            "first line\nsecond line",
            "界e\u{301}",
            "› leading glyph",
        ] {
            let (mut app, pane, input) = fixture();
            if bracketed {
                screen(&app, pane, "\x1b[?2004h");
            }
            let (rx, _) = start(&mut app, pane, json!({"text":text}));
            let paste = if bracketed {
                format!("\x1b[200~{text}\x1b[201~")
            } else {
                text.into()
            };
            bytes(&input, paste.as_bytes());
            assert!(
                rx.try_recv().is_err(),
                "queue admission is not confirmation"
            );
            screen(&app, pane, text.split('\n').next().unwrap());
            app.tick_agent_workflows(Instant::now());
            bytes(&input, b"\r");
            let value = response(&rx);
            assert_eq!(value["result"]["submission"], "confirmed");
            assert_eq!(value["result"]["evidence"], "input_echoed");
            assert_eq!(value["result"]["reason"], Value::Null);
            assert!(app.agent_prompts.is_empty());
        }
    }
}

#[test]
fn confirmed_prompt_does_not_match_transcript_substring_or_unchanged_input() {
    let _env = crate::persist::test_env("echo-collision");
    for initial in ["\x1b[2J\x1b[Hunique\r\n\r\n› ", "\x1b[2J\x1b[H\r\n› unique"] {
        let (mut app, pane, input) = fixture();
        screen(&app, pane, initial);
        let (rx, _) = start(&mut app, pane, json!({"text":"unique"}));
        bytes(&input, b"unique");
        app.tick_agent_workflows(Instant::now());
        assert!(rx.try_recv().is_err());
        timeout(&mut app);
        assert_eq!(response(&rx)["error"]["code"], "input_not_echoed");
        assert!(input.try_recv().is_err());
    }
}

#[test]
fn confirmed_prompt_exact_wrap_join_not_contains() {
    let _env = crate::persist::test_env("echo-wrap");
    for length in [78, 79, 80, 81, 159, 240] {
        let (mut app, pane, input) = fixture();
        app.panes[&pane].engine.lock().unwrap().resize(80, 24);
        let text = "X".repeat(length);
        let (rx, _) = start(&mut app, pane, json!({"text":text}));
        bytes(&input, text.as_bytes());
        screen(&app, pane, &text);
        app.tick_agent_workflows(Instant::now());
        bytes(&input, b"\r");
        assert_eq!(response(&rx)["result"]["submission"], "confirmed");
    }
}

#[test]
fn confirmed_prompt_multiline_and_truncation_require_exact_capacity() {
    let _env = crate::persist::test_env("echo-capacity");
    for (text, visible, succeeds) in [
        ("alpha\nbeta".into(), "alpha\r\nbeta".into(), true),
        ("alpha\nbeta".into(), "alp".into(), false),
        ("X".repeat(700), "X".repeat(640), true),
        ("X".repeat(700), "X".repeat(639), false),
        ("uniqueness".into(), "unique".into(), false),
        ("Unique".into(), "unique".into(), false),
    ] {
        let (mut app, pane, input) = fixture();
        app.panes[&pane].engine.lock().unwrap().resize(80, 24);
        screen(
            &app,
            pane,
            if text == "alpha\nbeta" {
                "\x1b[2J\x1b[H\r\n› "
            } else {
                "\x1b[2J\x1b[H"
            },
        );
        let (rx, _) = start(&mut app, pane, json!({"text":text}));
        bytes(&input, text.as_bytes());
        screen(&app, pane, &visible);
        app.tick_agent_workflows(Instant::now());
        if succeeds {
            bytes(&input, b"\r");
            assert_eq!(response(&rx)["result"]["submission"], "confirmed");
        } else {
            timeout(&mut app);
            assert_eq!(response(&rx)["error"]["code"], "input_not_echoed");
            assert!(input.try_recv().is_err());
        }
    }
}

#[test]
fn unconfirmed_prompt_no_confirm_keeps_atomic_submit_and_queued_evidence() {
    let _env = crate::persist::test_env("echo-opt-out");
    let (mut app, pane, input) = fixture();
    let (rx, _) = start(&mut app, pane, json!({"text":"old timing","confirm":false}));
    let InputAction::Submit { paste, .. } = input.try_recv().unwrap() else {
        panic!("atomic submit")
    };
    assert_eq!(paste, b"old timing");
    assert!(input.try_recv().is_err());
    assert_eq!(
        response(&rx)["result"],
        json!({
            "type":"agent_prompt", "pane":pane.0.to_string(), "submitted":true,
            "matched":false, "status":"idle", "baseline_revision":app.panes[&pane].content_revision(),
            "content_revision":app.panes[&pane].content_revision(), "evidence":"queued",
            "submission":"unconfirmed", "reason":null
        })
    );
}

#[test]
fn confirmed_prompt_pane_exit_during_echo_and_after_enter_releases_ownership() {
    let _env = crate::persist::test_env("echo-exit");
    for (confirm, entered) in [(true, false), (true, true), (false, true)] {
        for remove in [false, true] {
            let (mut app, pane, input) = fixture();
            let (rx, _) = start(
                &mut app,
                pane,
                json!({"text":"unique","confirm":confirm,"wait":true}),
            );
            input.try_recv().unwrap();
            if confirm && entered {
                screen(&app, pane, "unique");
                app.tick_agent_workflows(Instant::now());
                bytes(&input, b"\r");
            }
            if remove {
                app.panes.remove(&pane);
                app.tick_agent_workflows(Instant::now());
            } else {
                app.cancel_agent_waits(pane);
            }
            let value = response(&rx);
            assert_eq!(value["error"]["code"], "agent_not_running");
            assert_eq!(value["error"]["data"]["submitted"], entered);
            assert_eq!(
                value["error"]["data"]["submission"],
                if !entered {
                    "failed"
                } else if confirm {
                    "confirmed"
                } else {
                    "unconfirmed"
                }
            );
            assert_eq!(value["error"]["data"]["pane"], pane.0.to_string());
            assert!(app.agent_prompts.is_empty());
            assert!(input.try_recv().is_err());
        }
    }
}

#[test]
fn confirmed_prompt_overlap_during_echo_is_busy_and_cancel_releases() {
    let _env = crate::persist::test_env("echo-owner");
    let (mut app, pane, input) = fixture();
    let (first, cancel) = start(&mut app, pane, json!({"text":"first"}));
    bytes(&input, b"first");
    for confirm in [true, false] {
        let (second, _) = start(&mut app, pane, json!({"text":"second","confirm":confirm}));
        assert_eq!(response(&second)["error"]["code"], "agent_prompt_busy");
    }
    assert_eq!(
        app.dispatch(
            "agent.send",
            &json!({"target":pane.0.to_string(),"text":"second"})
        )
        .unwrap_err()
        .0,
        "agent_prompt_busy"
    );
    assert!(input.try_recv().is_err());
    cancel.store(true, Ordering::Release);
    screen(&app, pane, "first");
    app.tick_agent_workflows(Instant::now());
    assert!(app.agent_prompts.is_empty());
    assert!(first.try_recv().is_err());
    assert!(input.try_recv().is_err());
}

#[test]
fn confirmed_prompt_rejected_requests_never_queue_input() {
    let _env = crate::persist::test_env("echo-validation");
    let (mut app, pane, input) = fixture();
    for params in [
        json!({"text":""}),
        json!({"text":"x".repeat(MAX_AGENT_PROMPT_CHARS+1)}),
        json!({"text":"x","until":"idle"}),
        json!({"text":"x","timeout_s":1}),
        json!({"text":"x","confirm":"false"}),
    ] {
        let (rx, _) = start(&mut app, pane, params);
        assert_eq!(response(&rx)["error"]["code"], "invalid_request");
        assert!(input.try_recv().is_err());
        assert!(app.agent_prompts.is_empty());
    }
    app.status.get_mut(&pane).unwrap().agent = "bash".into();
    let (rx, _) = start(&mut app, pane, json!({"text":"no"}));
    assert_eq!(response(&rx)["error"]["code"], "agent_not_ready");
    app.panes.remove(&pane);
    let (rx, _) = start(&mut app, pane, json!({"text":"no"}));
    assert_eq!(response(&rx)["error"]["code"], "not_found");
    assert!(input.try_recv().is_err());
}

#[test]
fn confirmed_prompt_paste_and_enter_queue_failures_release_without_retry() {
    let _env = crate::persist::test_env("echo-send-failure");
    for enter in [false, true] {
        let (mut app, pane, input) = fixture();
        let rx = if enter {
            let (rx, _) = start(&mut app, pane, json!({"text":"unique"}));
            bytes(&input, b"unique");
            drop(input);
            screen(&app, pane, "unique");
            app.tick_agent_workflows(Instant::now());
            rx
        } else {
            drop(input);
            start(&mut app, pane, json!({"text":"unique"})).0
        };
        let value = response(&rx);
        assert_eq!(value["error"]["code"], "send_failed");
        assert_eq!(value["error"]["data"]["submitted"], false);
        assert_eq!(value["error"]["data"]["queued"], enter);
        assert_eq!(value["error"]["data"]["submission"], "failed");
        assert!(app.agent_prompts.is_empty());
    }
}

#[test]
fn confirmed_prompt_wait_starts_after_echo_and_preserves_until_and_timeout() {
    let _env = crate::persist::test_env("echo-until");
    for expires in [false, true] {
        let (mut app, pane, input) = fixture();
        let (rx, _) = start(
            &mut app,
            pane,
            json!({"text":"unique","wait":true,"until":["working"],"timeout_s":10}),
        );
        bytes(&input, b"unique");
        app.status.get_mut(&pane).unwrap().state = State::Working;
        screen(&app, pane, "unique");
        app.tick_agent_workflows(Instant::now());
        bytes(&input, b"\r");
        screen(&app, pane, "\x1b]0;unrelated post-Enter title\x07");
        app.tick_agent_workflows(Instant::now());
        assert!(
            rx.try_recv().is_err(),
            "pre-Enter Working is not phase B evidence"
        );
        if expires {
            app.tick_agent_workflows(Instant::now() + Duration::from_secs(11));
        } else {
            app.status.get_mut(&pane).unwrap().state = State::Idle;
            app.tick_agent_workflows(Instant::now());
            app.status.get_mut(&pane).unwrap().state = State::Working;
            app.tick_agent_workflows(Instant::now());
        }
        let value = response(&rx);
        assert_eq!(value["result"]["matched"], !expires);
        assert_eq!(value["result"]["submission"], "confirmed");
        assert_eq!(
            value["result"]["evidence"],
            if expires { "timeout" } else { "input_echoed" }
        );
        assert!(input.try_recv().is_err());
        assert!(app.agent_prompts.is_empty());
    }
}

#[test]
fn confirmed_prompt_full_input_queue_never_sends_enter() {
    let _env = crate::persist::test_env("echo-full-queue");
    let (mut app, pane, _) = fixture();
    let _held = app
        .panes
        .get_mut(&pane)
        .unwrap()
        .fill_input_queue_for_test();
    let (rx, _) = start(&mut app, pane, json!({"text":"unique"}));
    let value = response(&rx);
    assert_eq!(value["error"]["code"], "send_failed");
    assert_eq!(value["error"]["data"]["queued"], false);
    assert_eq!(value["error"]["data"]["submitted"], false);
    assert!(app.agent_prompts.is_empty());
}

#[test]
fn confirmed_prompt_capacity_failure_queues_nothing() {
    let _env = crate::persist::test_env("echo-capacity-limit");
    let (mut app, pane, input) = fixture();
    for _ in 0..MAX_AGENT_WAITS_TOTAL {
        let (_rx, _) = start(&mut app, pane, json!({"text":"unique"}));
        bytes(&input, b"unique");
        let owner = app.agent_prompts.remove(&pane).unwrap();
        // Zero is never a live pane ID; accumulate independent occupied slots.
        app.agent_prompts
            .entry(PaneId(0))
            .or_default()
            .extend(owner);
    }
    let (rx, _) = start(&mut app, pane, json!({"text":"unique"}));
    assert_eq!(response(&rx)["error"]["code"], "unavailable");
    assert!(input.try_recv().is_err());
    assert_eq!(
        app.agent_prompts.values().map(Vec::len).sum::<usize>(),
        MAX_AGENT_WAITS_TOTAL
    );
}

#[test]
fn confirmed_prompt_wait_timeout_before_echo_never_sends_enter() {
    let _env = crate::persist::test_env("echo-short-deadline");
    let (mut app, pane, input) = fixture();
    let (rx, _) = start(
        &mut app,
        pane,
        json!({"text":"unique","wait":true,"timeout_s":0}),
    );
    bytes(&input, b"unique");
    screen(&app, pane, "unique");
    app.tick_agent_workflows(Instant::now());
    assert_eq!(response(&rx)["error"]["code"], "input_not_echoed");
    assert!(input.try_recv().is_err());
    assert!(app.agent_prompts.is_empty());
}
