use super::types::AgentDescriptor;

pub(crate) static BUILTINS: &[&AgentDescriptor] = &[
    &super::claude::DESCRIPTOR,
    &super::codex::DESCRIPTOR,
    &super::gemini::DESCRIPTOR,
    &super::antigravity::DESCRIPTOR,
    &super::aider::DESCRIPTOR,
    &super::opencode::DESCRIPTOR,
    &super::copilot::DESCRIPTOR,
    &super::kimi::DESCRIPTOR,
    &super::qwen::DESCRIPTOR,
    &super::kiro::DESCRIPTOR,
    &super::cursor::DESCRIPTOR,
    &super::amp::DESCRIPTOR,
    &super::droid::DESCRIPTOR,
    &super::grok::DESCRIPTOR,
    &super::hermes::DESCRIPTOR,
    &super::muse::DESCRIPTOR,
    &super::omp::DESCRIPTOR,
    &super::pi::DESCRIPTOR,
    &super::fx::DESCRIPTOR,
];

// Preserve the current Settings and CLI presentation order independently of
// identity precedence. Support still comes from each agent's descriptor.
static INTEGRATIONS: &[&AgentDescriptor] = &[
    &super::claude::DESCRIPTOR,
    &super::copilot::DESCRIPTOR,
    &super::codex::DESCRIPTOR,
    &super::antigravity::DESCRIPTOR,
    &super::opencode::DESCRIPTOR,
    &super::kimi::DESCRIPTOR,
    &super::grok::DESCRIPTOR,
    &super::hermes::DESCRIPTOR,
    &super::omp::DESCRIPTOR,
    &super::pi::DESCRIPTOR,
];

const _: () = assert!(
    super::omp::DESCRIPTOR.identity.overlap_priority
        > super::pi::DESCRIPTOR.identity.overlap_priority
);

pub(crate) fn descriptors() -> &'static [&'static AgentDescriptor] {
    BUILTINS
}

pub(crate) fn integrations() -> &'static [&'static AgentDescriptor] {
    debug_assert!(INTEGRATIONS
        .iter()
        .all(|descriptor| descriptor.integration.is_some()));
    INTEGRATIONS
}

pub(crate) fn find(name: &str) -> Option<&'static AgentDescriptor> {
    BUILTINS.iter().copied().find(|descriptor| {
        descriptor.id.eq_ignore_ascii_case(name)
            || descriptor
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn builtins_have_unique_ids_and_aliases() {
        let mut names = HashSet::new();
        for descriptor in BUILTINS {
            assert!(
                names.insert(descriptor.id),
                "duplicate agent id {}",
                descriptor.id
            );
            for alias in descriptor.aliases {
                assert!(
                    names.insert(alias),
                    "duplicate agent alias {alias} for {}",
                    descriptor.id
                );
            }
            assert!(
                !descriptor.launch_command.is_empty()
                    && !descriptor.launch_command.chars().any(char::is_whitespace),
                "invalid launch command for {}",
                descriptor.id
            );
            assert!(
                descriptor
                    .task_prompt_args
                    .iter()
                    .all(|arg| { !arg.is_empty() && !arg.chars().any(char::is_whitespace) }),
                "invalid task prompt argument for {}",
                descriptor.id
            );
            if let Some(automation) = descriptor.automation {
                let launches = [
                    automation.read_only,
                    automation.workspace,
                    automation.full_access,
                ];
                assert!(
                    launches.iter().any(Option::is_some),
                    "{} advertises automation without a launch profile",
                    descriptor.id
                );
                for launch in launches.into_iter().flatten() {
                    assert!(
                        launch
                            .args
                            .iter()
                            .all(|arg| !arg.is_empty() && !arg.chars().any(char::is_control)),
                        "invalid automation argument for {}",
                        descriptor.id
                    );
                }
            }
        }
    }

    #[test]
    fn automation_capabilities_are_explicit_per_agent_and_access_level() {
        use crate::automation::AutomationAccess;

        let codex = find("codex").unwrap().automation.unwrap();
        assert!(codex.supports(AutomationAccess::ReadOnly));
        assert!(codex.supports(AutomationAccess::Workspace));
        assert!(codex.supports(AutomationAccess::FullAccess));

        let aider = find("aider").unwrap().automation.unwrap();
        assert!(aider.supports(AutomationAccess::ReadOnly));
        assert!(!aider.supports(AutomationAccess::Workspace));
        assert!(aider.supports(AutomationAccess::FullAccess));

        assert!(find("antigravity").unwrap().automation.is_none());
        assert!(find("amp").unwrap().automation.is_none());

        let pi = find("pi").unwrap().automation.unwrap();
        assert!(pi.supports(AutomationAccess::ReadOnly));
        assert!(!pi.supports(AutomationAccess::Workspace));
        assert!(!pi.supports(AutomationAccess::FullAccess));
    }

    #[test]
    fn omp_precedes_pi_with_stronger_overlap_priority() {
        let omp = BUILTINS.iter().position(|agent| agent.id == "omp").unwrap();
        let pi = BUILTINS.iter().position(|agent| agent.id == "pi").unwrap();
        assert!(omp < pi);
        assert!(BUILTINS[omp].identity.overlap_priority > BUILTINS[pi].identity.overlap_priority);
        assert_eq!(
            BUILTINS[omp].identity.interpreter_packages,
            ["@oh-my-pi/pi-coding-agent"]
        );
        assert_eq!(
            BUILTINS[pi].identity.interpreter_packages,
            ["@earendil-works/pi-coding-agent"]
        );
    }

    #[test]
    fn interpreter_package_identities_are_unique() {
        let mut packages = HashSet::new();
        for descriptor in BUILTINS {
            for package in descriptor.identity.interpreter_packages {
                assert_eq!(*package, package.to_lowercase());
                assert!(
                    packages.insert(*package),
                    "duplicate interpreter package {package} for {}",
                    descriptor.id
                );
            }
        }
    }

    #[test]
    fn aliases_resolve_to_the_canonical_descriptor() {
        assert_eq!(find("cursor-agent").map(|agent| agent.id), Some("cursor"));
        assert_eq!(find("CURSOR").map(|agent| agent.id), Some("cursor"));
        assert_eq!(find("agy").map(|agent| agent.id), Some("antigravity"));
        assert_eq!(
            find("ANTIGRAVITY-CLI").map(|agent| agent.id),
            Some("antigravity")
        );
        assert!(find("not-an-agent").is_none());
    }

    #[test]
    fn builtin_identity_projection_preserves_the_native_table() {
        let actual: Vec<_> = BUILTINS
            .iter()
            .map(|descriptor| {
                (
                    descriptor.id,
                    descriptor.identity.distinct,
                    descriptor.identity.ambiguous,
                )
            })
            .collect();
        let expected = vec![
            ("claude", &["claude"][..], &[][..]),
            ("codex", &["codex"][..], &[][..]),
            ("gemini", &["gemini"][..], &[][..]),
            ("antigravity", &["antigravity-cli"][..], &["agy"][..]),
            ("aider", &["aider"][..], &[][..]),
            ("opencode", &["opencode"][..], &[][..]),
            ("copilot", &["copilot"][..], &[][..]),
            ("kimi", &["kimi"][..], &[][..]),
            ("qwen", &["qwen"][..], &[][..]),
            ("kiro", &["kiro"][..], &[][..]),
            ("cursor", &["cursor-agent"][..], &["cursor"][..]),
            ("amp", &[][..], &["amp"][..]),
            ("droid", &[][..], &["droid"][..]),
            ("grok", &[][..], &["grok"][..]),
            (
                "hermes",
                &["hermes-agent", "hermes-cli", "hermes_cli.main"][..],
                &["hermes"][..],
            ),
            (
                "muse",
                &["muse-code", "muse-cli", "muse code"][..],
                &["muse"][..],
            ),
            ("omp", &["oh-my-pi", "omp-coding-agent"][..], &["omp"][..]),
            ("pi", &["pi-coding-agent"][..], &["pi"][..]),
            ("fx", &[][..], &["fx"][..]),
        ];
        assert_eq!(actual, expected);
        assert!(BUILTINS.iter().all(|descriptor| {
            !descriptor.identity.distinct.is_empty()
                || !descriptor.identity.ambiguous.is_empty()
                || descriptor.identity.binary_matcher.is_some()
        }));
    }

    #[test]
    fn native_capability_projection_preserves_current_support() {
        let resumable: Vec<_> = BUILTINS
            .iter()
            .filter(|descriptor| descriptor.sessions.is_some())
            .map(|descriptor| descriptor.id)
            .collect();
        assert_eq!(
            resumable,
            [
                "claude",
                "codex",
                "gemini",
                "antigravity",
                "opencode",
                "copilot",
                "kimi",
                "qwen",
                "cursor",
                "grok",
                "hermes",
                "muse",
                "omp",
                "pi",
                "fx",
            ]
        );

        let discoverable: Vec<_> = BUILTINS
            .iter()
            .filter(|descriptor| {
                descriptor
                    .sessions
                    .as_ref()
                    .and_then(|sessions| sessions.discovery.as_ref())
                    .is_some()
            })
            .map(|descriptor| descriptor.id)
            .collect();
        assert_eq!(
            discoverable,
            [
                "claude",
                "codex",
                "gemini",
                "antigravity",
                "opencode",
                "copilot",
                "kimi",
                "qwen",
                "grok",
                "muse",
                "omp",
                "pi",
                "fx",
            ]
        );

        let forkable: Vec<_> = BUILTINS
            .iter()
            .filter(|descriptor| {
                descriptor
                    .sessions
                    .as_ref()
                    .and_then(|sessions| sessions.fork)
                    .is_some()
            })
            .map(|descriptor| descriptor.id)
            .collect();
        assert_eq!(forkable, ["claude", "codex", "grok", "omp", "pi"]);

        assert_eq!(
            integrations()
                .iter()
                .map(|descriptor| descriptor.id)
                .collect::<Vec<_>>(),
            [
                "claude",
                "copilot",
                "codex",
                "antigravity",
                "opencode",
                "kimi",
                "grok",
                "hermes",
                "omp",
                "pi",
            ]
        );
    }
}
