use super::types::{AgentDescriptor, IdentityDescriptor};

pub(super) const DESCRIPTOR: AgentDescriptor = AgentDescriptor {
    id: "command-code",
    aliases: &["commandcode"],
    launch_command: "command-code",
    task_prompt_args: &[],
    automation: None,
    identity: IdentityDescriptor {
        distinct: &["command-code", "commandcode"],
        ambiguous: &[],
        binary_matcher: None,
        interpreter_packages: &["command-code"],
        interpreter_scripts: &["cmd", "cmdc"],
        overlap_priority: 0,
    },
    sessions: None,
    integration: None,
};

#[cfg(test)]
mod tests {
    use crate::detect::{classify, Detection, Manifests};
    use crate::ui::theme::State;

    const IDLE_STARTED: &str = include_str!("fixtures/idle_started.txt");
    const WORKING: &str = include_str!("fixtures/working.txt");
    const PERMISSION: &str = include_str!("fixtures/permission.txt");
    const IDLE_AFTER_TURN: &str = include_str!("fixtures/idle_after_turn.txt");
    const PLAN_WARNING: &str = include_str!("fixtures/plan_warning.txt");
    const INSUFFICIENT_CREDITS: &str = include_str!("fixtures/insufficient_credits.txt");

    const WORKING_LINE: &str = " · Investigating…  esc to interrupt • 0s • ↓ 0";
    const WORKED_LINE: &str = "✻ Worked for 1s";
    const SPINNER_ANSWER: &str = "⠶ ok";

    fn running() -> Vec<String> {
        vec!["node /usr/bin/cmd --permission-mode default".to_string()]
    }

    fn detect(screen: &str) -> Detection {
        classify(
            None,
            screen,
            true,
            false,
            "cmd",
            "",
            &running(),
            &Manifests::builtin(),
        )
    }

    fn agent_of(running: &[&str]) -> String {
        let running: Vec<String> = running.iter().map(|line| line.to_string()).collect();
        classify(
            None,
            "",
            false,
            false,
            "x",
            "",
            &running,
            &Manifests::builtin(),
        )
        .agent
    }

    fn without(screen: &str, needle: &str) -> String {
        assert!(screen.contains(needle), "fixture lacks {needle:?}");
        screen
            .lines()
            .filter(|line| !line.contains(needle))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn identity_from_process_names_and_package() {
        for argv in [
            "command-code --yolo",
            "/usr/local/bin/commandcode -t",
            "node /home/u/.nvm/bin/cmd --permission-mode default",
            "node --no-warnings /usr/bin/cmd",
            "bun /opt/bin/cmd",
            "nodejs /usr/bin/cmd",
            "node /usr/bin/cmdc",
            "bun /opt/bin/cmdc",
            "node /usr/lib/node_modules/command-code/dist/index.mjs",
            r"node C:\Users\u\AppData\Roaming\npm\node_modules\command-code\dist\index.mjs",
            "/usr/bin/env node /usr/bin/cmd",
        ] {
            assert_eq!(agent_of(&[argv]), "command-code", "{argv}");
        }
    }

    #[test]
    fn cmd_alone_is_not_command_code() {
        for argv in [
            "cmd",
            "/usr/bin/cmd",
            r"C:\Windows\System32\cmd.exe /c dir",
            "cmd.exe",
            "bash ./cmd",
            "python3 /opt/cmd",
            "node ./cmd.js",
            "node /w/tools/cmd.mjs",
            "node /w/tools/cmd-runner",
            "node /usr/bin/cmdcx",
            "node /usr/bin/cmdc.js",
            "cmdc",
            "/usr/bin/cmdc",
            "deno run /usr/bin/cmd",
            "deno /usr/bin/cmd",
            "node /w/node_modules/command-code-extras/index.js",
            "cargo run --example command-code",
        ] {
            assert_eq!(agent_of(&[argv]), "x", "{argv}");
        }
    }

    #[test]
    fn steady_state_process_title_is_identified() {
        for argv in ["command-code", "command-code --permission-mode default"] {
            assert_eq!(agent_of(&[argv]), "command-code", "{argv}");
        }
        assert_eq!(agent_of(&["node /usr/bin/cmd"]), "command-code");
    }

    #[test]
    fn manifest_identity_reaches_the_script_rule() {
        let apply = |toml: &str, m: &mut Manifests| {
            m.apply_manifest_for_test(toml);
        };
        let named = |m: &Manifests, argv: &str| {
            classify(None, "", false, false, "x", "", &[argv.to_string()], m).agent
        };
        let mut kept = Manifests::builtin();
        apply(
            "agent = \"command-code\"\n[identity]\ndistinct = [\"command-code-extra\"]\n",
            &mut kept,
        );
        assert_eq!(named(&kept, "node /usr/bin/cmd"), "command-code");
        assert_eq!(named(&kept, "command-code-extra"), "command-code");

        let mut replaced = Manifests::builtin();
        apply(
            "agent = \"command-code\"\n[identity]\ndistinct = [\"command-code\"]\nreplace = true\n",
            &mut replaced,
        );
        assert_eq!(named(&replaced, "node /usr/bin/cmd"), "x");
        assert_eq!(named(&replaced, "node /usr/bin/cmdc"), "x");
        assert_eq!(named(&replaced, "command-code"), "command-code");

        let mut unknown = Manifests::builtin();
        apply(
            "agent = \"nimbus\"\n[identity]\ndistinct = [\"nimbus-cli\"]\n",
            &mut unknown,
        );
        assert_eq!(named(&unknown, "nimbus-cli"), "nimbus");
        assert_eq!(named(&unknown, "node /usr/bin/cmd"), "command-code");
        assert_eq!(named(&unknown, "node /usr/bin/nimbus"), "x");
    }

    #[test]
    fn launch_args_follow_the_runtime() {
        let manifests = Manifests::builtin();
        for runtime in ["node", "nodejs", "bun"] {
            let running = vec![format!("{runtime} /usr/bin/cmd --yolo -t")];
            assert_eq!(
                manifests.launch_args_for(&running, "command-code"),
                Some(vec!["--yolo".to_string(), "-t".to_string()]),
                "{runtime}"
            );
        }
        let other = vec!["bash /usr/bin/cmd --yolo".to_string()];
        assert_eq!(manifests.launch_args_for(&other, "command-code"), None);
    }

    #[test]
    fn finished_turn_needs_the_glyph() {
        let screen = IDLE_AFTER_TURN.replace("✻ Worked for", "Worked for");
        let found = detect(&screen);
        assert_eq!(found.state, State::Working);
        assert_eq!(found.rule_priority, Some(110));
    }

    #[test]
    fn cmd_in_title_launch_or_screen_text_is_not_command_code() {
        let manifests = Manifests::builtin();
        for (title, screen, base) in [
            (Some("cmd"), "", "zsh"),
            (None, "", "cmd"),
            (None, "C:\\Users\\u> cmd\n", "cmd.exe"),
            (None, "run cmd to start\n", "zsh"),
        ] {
            let found = classify(title, screen, false, false, base, "", &[], &manifests);
            assert_ne!(found.agent, "command-code", "{title:?} {screen:?} {base:?}");
        }
    }

    #[test]
    fn package_scope_and_name_stay_exact() {
        assert_eq!(
            agent_of(&["node /x/node_modules/command-code/dist/index.mjs"]),
            "command-code"
        );
        assert_eq!(
            agent_of(&["node /x/node_modules/command-codes/dist/index.mjs"]),
            "x"
        );
    }

    #[test]
    fn idle_screens_are_idle_not_flapping() {
        for (name, screen) in [
            ("idle_started", IDLE_STARTED),
            ("insufficient_credits", INSUFFICIENT_CREDITS),
        ] {
            let found = detect(screen);
            assert_eq!(found.agent, "command-code", "{name}");
            assert_eq!(found.state, State::Idle, "{name}");
            assert_eq!(found.state_source, "no_positive_state_evidence", "{name}");
        }
    }

    #[test]
    fn working_screen_is_working() {
        let found = detect(WORKING);
        assert_eq!(found.state, State::Working);
        assert_eq!(found.state_source, "manifest_rule");
        assert_eq!(found.rule_priority, Some(100));
    }

    #[test]
    fn finished_turn_is_idle_not_working() {
        for screen in [IDLE_AFTER_TURN, PLAN_WARNING] {
            let found = detect(screen);
            assert_eq!(found.state, State::Idle);
            assert_eq!(found.rule_priority, Some(115));
            assert_eq!(found.rule_region, Some("screen"));
        }
    }

    #[test]
    fn finished_turn_needs_the_worked_line() {
        let screen = without(IDLE_AFTER_TURN, WORKED_LINE);
        assert!(screen.contains(SPINNER_ANSWER));
        let found = detect(&screen);
        assert_eq!(found.state, State::Working);
        assert_eq!(found.rule_priority, Some(110));
    }

    #[test]
    fn finished_turn_needs_the_input_box() {
        let screen = without(IDLE_AFTER_TURN, "Ask your question");
        let found = detect(&screen);
        assert_eq!(found.state, State::Working);
        assert_eq!(found.rule_priority, Some(110));
    }

    #[test]
    fn next_turn_is_working_while_the_old_worked_line_is_visible() {
        let screen = IDLE_AFTER_TURN.replace(
            "────────────────────────────────────────────────────────────────────────────────\n❯ Ask",
            &format!("{WORKING_LINE}\n────────────────────────────────────────────────────────────────────────────────\n❯ Ask"),
        );
        assert!(screen.contains(WORKED_LINE) && screen.contains("esc to interrupt"));
        let found = detect(&screen);
        assert_eq!(found.state, State::Working);
        assert_ne!(found.rule_priority, Some(115));
    }

    #[test]
    fn permission_prompt_is_blocked_by_its_own_rule() {
        let found = detect(PERMISSION);
        assert_eq!(found.state, State::Blocked);
        assert_eq!(found.rule_priority, Some(310));
        assert_eq!(found.rule_region, Some("screen"));
    }

    #[test]
    fn permission_rule_needs_both_footer_parts() {
        for needle in ["to bypass all permissions", "enter select"] {
            let screen = PERMISSION.replace(needle, "x");
            let found = detect(&screen);
            assert_ne!(found.rule_priority, Some(310), "{needle}");
        }
    }

    #[test]
    fn permission_rule_holds_when_the_selection_moves() {
        let screen = PERMISSION
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("  2. Yes, allow", "❯ 2. Yes, allow");
        let found = detect(&screen);
        assert_eq!(found.state, State::Blocked);
        assert_eq!(found.rule_priority, Some(310));
    }

    #[test]
    fn permission_hint_in_the_transcript_alone_is_not_blocked() {
        let screen = IDLE_STARTED.replace(
            "# /var/tmp/cmdcap/work/w1",
            "# /var/tmp/cmdcap/work/w1\nRun cmd --yolo to bypass all permissions",
        );
        let found = detect(&screen);
        assert_eq!(found.state, State::Idle);
    }

    #[test]
    fn rules_do_not_apply_to_other_agents() {
        let manifests = Manifests::builtin();
        let other = classify(
            None,
            IDLE_AFTER_TURN,
            false,
            false,
            "claude",
            "",
            &["claude".to_string()],
            &manifests,
        );
        assert_ne!(other.rule_priority, Some(115));
    }

    #[test]
    fn registry_entry_is_unique_and_resolves() {
        let found = crate::agent::registry::find("commandcode").expect("alias");
        assert_eq!(found.id, "command-code");
        assert!(crate::agent::registry::find("cmd").is_none());
    }
}
