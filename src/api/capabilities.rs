use serde_json::{json, Value};

/// Canonical methods and compatibility aliases accepted by the live server.
/// Keep this registry in lockstep with dispatch and the installed schema.
pub const METHODS: &[&str] = &[
    "uhp.capabilities",
    "uhp.stats",
    "uhp.token.create",
    "uhp.token.list",
    "uhp.token.revoke",
    "ping",
    "server.stop",
    "server.reload_config",
    "server.agent_manifests",
    "server.reload_agent_manifests",
    "config.get",
    "config.patch",
    "session.snapshot",
    "events.subscribe",
    "events.wait",
    "wait.output",
    "workspace.list",
    "workspace.get",
    "workspace.new",
    "workspace.open",
    "workspace.focus",
    "workspace.rename",
    "workspace.pin",
    "workspace.move",
    "workspace.move_block",
    "workspace.report_metadata",
    "workspace.close",
    "node.list",
    "node.new",
    "node.open",
    "node.focus",
    "node.rename",
    "node.pin",
    "node.close",
    "tab.list",
    "tab.get",
    "tab.new",
    "tab.focus",
    "tab.move",
    "tab.swap",
    "tab.rename",
    "tab.close",
    "pane.list",
    "pane.get",
    "pane.current",
    "pane.layout",
    "pane.neighbor",
    "pane.edges",
    "pane.split",
    "pane.move",
    "pane.swap",
    "pane.focus",
    "pane.focus_direction",
    "pane.resize",
    "pane.zoom",
    "pane.rename",
    "pane.run",
    "pane.send_input",
    "pane.read",
    "pane.status",
    "pane.processes",
    "pane.report_session",
    "pane.report_event",
    "pane.close",
    "attach.pane",
    "layout.export",
    "layout.apply",
    "layout.set_split_ratio",
    "agent.list",
    "agent.get",
    "agent.explain",
    "agent.report",
    "agent.release",
    "agent.start",
    "agent.prompt",
    "agent.wait",
    "agent.name",
    "agent.fork",
    "agent.send",
    "agent.keys",
    "agent.read",
    "agent.sessions",
    "agent.resume",
    "search",
    "search.capabilities",
    "search.query",
    "search.activate",
    "files.tree",
    "files.open",
    "files.reveal",
    "files.refresh",
    "git.status",
    "git.branches",
    "git.log",
    "git.open",
    "mission.snapshot",
    "mission.refresh",
    "mission.open",
    "diff.refresh",
    "diff.list",
    "diff.open",
    "diff.get",
    "diff.navigate",
    "diff.note.add",
    "diff.note.apply",
    "diff.note.list",
    "diff.note.edit",
    "diff.note.resolve",
    "diff.note.reopen",
    "diff.note.remove",
    "diff.note.send",
    "worktree.list",
    "worktree.create",
    "worktree.open",
    "worktree.remove",
    "task.add",
    "task.list",
    "task.get",
    "task.claim",
    "task.next",
    "task.start",
    "task.heartbeat",
    "task.update",
    "task.done",
    "task.merge",
    "task.release",
    "task.delete",
    "automation.create",
    "automation.list",
    "automation.get",
    "automation.update",
    "automation.enable",
    "automation.disable",
    "automation.rebind",
    "automation.delete",
    "automation.run",
    "automation.history",
    "automation.preview",
    "automation.health",
    "lease.acquire",
    "lease.list",
    "lease.release",
    "module.list",
    "module.info",
    "module.link",
    "module.unlink",
    "module.uninstall",
    "module.enable",
    "module.disable",
    "module.action.list",
    "module.action.invoke",
    "module.pane.open",
    "module.pane.focus",
    "module.pane.close",
    "module.config_dir",
    "module.settings.list",
    "module.settings.get",
    "module.settings.set",
    "module.log.list",
    "theme.list",
    "theme.path",
    "theme.use",
    "theme.reload",
    "manifest.reload",
    "ui.sidebar",
    "ui.dock.push",
    "ui.dock.list",
    "ui.dock.move",
    "ui.bar.push",
    "ui.bar.list",
    "ui.bar.move",
    "ui.bar.remove",
    "ui.notification.push",
    "ui.notification.clear",
    "ui.toast",
    "terminal.backend.inventory",
    "terminal.backend.snapshot",
    "terminal.backend.validate",
    "terminal.backend.processes",
    "terminal.backend.capture",
    "terminal.backend.observe",
    "terminal.backend.control",
    "terminal.backend.type_literal",
    "terminal.backend.submit_text",
    "terminal.backend.send_key",
    "terminal.backend.set_title",
    "terminal.backend.notify",
    "terminal.backend.create",
    "terminal.backend.close",
    "terminal.backend.wait_change",
    "terminal.backend.wait_output",
    "terminal.backend.events.subscribe",
];

const READ_ONLY_METHODS: &[&str] = &[
    "host.capabilities",
    "host.info",
    "host.doctor",
    "host.update.check",
    "uhp.capabilities",
    "uhp.stats",
    "uhp.token.list",
    "ping",
    "server.agent_manifests",
    "config.get",
    "session.snapshot",
    "session.list",
    "session.status",
    "skill.status",
    "integration.status",
    "events.subscribe",
    "events.wait",
    "wait.output",
    "workspace.list",
    "workspace.get",
    "node.list",
    "tab.list",
    "tab.get",
    "pane.list",
    "pane.get",
    "pane.current",
    "pane.layout",
    "pane.neighbor",
    "pane.edges",
    "pane.read",
    "pane.status",
    "pane.processes",
    "layout.export",
    "agent.list",
    "agent.get",
    "agent.explain",
    "agent.wait",
    "agent.read",
    "agent.sessions",
    "search",
    "search.capabilities",
    "search.query",
    "files.tree",
    "git.status",
    "git.branches",
    "git.log",
    "mission.snapshot",
    "mission.refresh",
    "diff.list",
    "diff.get",
    "diff.note.list",
    "worktree.list",
    "task.list",
    "task.get",
    "task.next",
    "automation.list",
    "automation.get",
    "automation.history",
    "automation.preview",
    "automation.health",
    "lease.list",
    "module.list",
    "module.info",
    "module.action.list",
    "module.config_dir",
    "module.settings.list",
    "module.settings.get",
    "module.log.list",
    "theme.list",
    "theme.path",
    "ui.dock.list",
    "ui.bar.list",
    "terminal.backend.inventory",
    "terminal.backend.snapshot",
    "terminal.backend.validate",
    "terminal.backend.processes",
    "terminal.backend.capture",
    "terminal.backend.observe",
    "terminal.backend.wait_change",
    "terminal.backend.wait_output",
    "terminal.backend.events.subscribe",
];

pub fn is_read_only(method: &str) -> bool {
    READ_ONLY_METHODS.contains(&method)
}

pub fn required_scope(method: &str) -> &'static str {
    if matches!(
        method,
        "host.capabilities"
            | "host.info"
            | "host.doctor"
            | "host.update.check"
            | "uhp.capabilities"
            | "uhp.stats"
            | "ping"
            | "session.snapshot"
            | "session.list"
            | "session.status"
            | "skill.status"
            | "integration.status"
            | "events.subscribe"
            | "events.wait"
            | "wait.output"
    ) {
        "read"
    } else if method.starts_with("terminal.backend.") {
        "terminal"
    } else if method.starts_with("agent.") || method.starts_with("pane.report_") {
        "agent"
    } else if method.starts_with("task.")
        || method.starts_with("lease.")
        || method.starts_with("automation.")
    {
        "orchestration"
    } else if method.starts_with("module.") {
        "extensions"
    } else if method.starts_with("workspace.")
        || method.starts_with("node.")
        || method.starts_with("tab.")
        || method.starts_with("pane.")
        || method.starts_with("layout.")
        || method.starts_with("search")
        || method.starts_with("files.")
        || method.starts_with("git.")
        || method.starts_with("mission.")
        || method.starts_with("diff.")
        || method.starts_with("worktree.")
    {
        "workspace"
    } else {
        "admin"
    }
}

#[cfg(test)]
pub fn all_methods() -> impl Iterator<Item = &'static str> {
    METHODS
        .iter()
        .copied()
        .chain(crate::api::host::METHODS.iter().copied())
}

fn is_idempotent(method: &str) -> bool {
    method == "automation.rebind"
        || (is_read_only(method)
            && !matches!(
                method,
                "events.subscribe"
                    | "terminal.backend.events.subscribe"
                    | "terminal.backend.observe"
            ))
}

#[cfg(test)]
fn method_contracts() -> Vec<Value> {
    method_contracts_for(METHODS.iter().copied())
}

fn method_contracts_for<'a>(methods: impl Iterator<Item = &'a str>) -> Vec<Value> {
    methods
        .map(|method| {
            let read_only = is_read_only(method);
            json!({
                "method":method,
                "access":if read_only { "read" } else { "write" },
                "scope":required_scope(method),
                "idempotent":is_idempotent(method),
            })
        })
        .collect()
}

pub fn capabilities(event_sequence: u64) -> Value {
    json!({
        "type":"uhp_capabilities",
        "protocol":{
            "name":super::PROTOCOL_NAME,
            "major":super::PROTOCOL_MAJOR,
            "minor":super::PROTOCOL_MINOR,
        },
        "event_sequence":event_sequence,
        "methods":METHODS,
        "method_contracts":method_contracts_for(METHODS.iter().copied()),
        "limits":{
            "frame_bytes":crate::terminal::backend::MAX_FRAME_BYTES,
            "event_queue":crate::ipc::api::event_queue_capacity(),
            "event_subscribers":crate::ipc::api::max_event_subscribers(),
            "event_replay":crate::ipc::api::event_replay_capacity(),
            "event_replay_bytes":crate::ipc::api::event_replay_bytes(),
            "active_connections":crate::ipc::api::active_connections(),
            "connection_capacity":crate::ipc::api::max_active_connections(),
            "rejected_connections":crate::ipc::api::rejected_connections(),
            "terminal_streams":crate::ipc::api::active_terminal_streams(),
            "terminal_stream_capacity":crate::terminal::backend::MAX_OBSERVERS,
            "terminal_stream_queue":crate::terminal::backend::OBSERVER_QUEUE_CAPACITY,
            "terminal_stream_frame_bytes":crate::terminal::backend::MAX_OBSERVE_BYTES,
            "event_wait_timeout_s":super::topology::MAX_EVENT_WAIT_S,
            "layout_depth":super::topology::MAX_LAYOUT_DEPTH,
            "workspace_move_block":super::topology::MAX_WORKSPACE_MOVE_BLOCK,
            "task_title_bytes":crate::orch::MAX_TASK_TITLE_BYTES,
            "task_prompt_bytes":crate::orch::MAX_TASK_PROMPT_BYTES,
            "automations":crate::automation::MAX_AUTOMATIONS,
            "automation_runs":crate::automation::MAX_RUNS,
            "automation_prompt_bytes":crate::automation::MAX_PROMPT_BYTES,
            "automation_gate_bytes":crate::automation::MAX_GATE_BYTES,
            "automation_min_interval_s":crate::automation::MIN_INTERVAL_SECONDS,
        },
        "identity":{"workspace":"stable","tab":"stable","terminal":"pty_lifetime"},
        "events":{"resume":"after_sequence","loss":"resync_required"},
        "agent_authorities":["integration_report","process_tree","launch_command","osc_title","screen_text","prior_identity","command_fallback"],
        "agent_states":["idle","working","blocked","done"],
        "terminal":{
            "capabilities":crate::terminal::backend::CAPABILITIES,
            "limits":crate::terminal::backend::limits_json(),
        },
        "authorization":{"default":"local_owner","delegation":"scoped_ephemeral_token",
            "scopes":["read","workspace","agent","terminal","orchestration","extensions","admin","all"]},
        "concurrency":{"mutation_guard":"if_revision"},
        "atomic_methods":["agent.start","agent.prompt","automation.create","automation.rebind","automation.run","workspace.move_block","layout.apply","diff.note.apply"],
        "idempotency_keys":{"methods":["automation.create","automation.run"],"max_bytes":128},
        "graphics":false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_no_duplicates_and_contains_required_surface() {
        let methods = all_methods().collect::<Vec<_>>();
        let unique: std::collections::BTreeSet<_> = methods.iter().copied().collect();
        assert_eq!(unique.len(), methods.len());
        for required in [
            "uhp.capabilities",
            "workspace.get",
            "pane.current",
            "layout.apply",
            "config.patch",
            "events.wait",
            "terminal.backend.observe",
            "terminal.backend.control",
            "automation.create",
            "automation.rebind",
            "automation.health",
        ] {
            assert!(unique.contains(required), "missing {required}");
        }
        assert!(READ_ONLY_METHODS
            .iter()
            .all(|method| unique.contains(method)));
        let contracts = method_contracts();
        assert_eq!(contracts.len(), METHODS.len());
        assert!(contracts.iter().all(|contract| {
            contract["access"].is_string()
                && contract["scope"].is_string()
                && contract["idempotent"].is_boolean()
        }));
        let capabilities = capabilities(0);
        assert_eq!(capabilities["limits"]["terminal_stream_capacity"], 8);
        assert_eq!(capabilities["limits"]["terminal_stream_queue"], 2);
        assert_eq!(
            capabilities["limits"]["task_title_bytes"],
            crate::orch::MAX_TASK_TITLE_BYTES
        );
        assert_eq!(
            capabilities["limits"]["task_prompt_bytes"],
            crate::orch::MAX_TASK_PROMPT_BYTES
        );
        assert!(is_idempotent("pane.list"));
        assert!(!is_read_only("mission.open"));
        assert_eq!(required_scope("mission.open"), "workspace");
        assert_eq!(required_scope("session.snapshot"), "read");
        assert_eq!(required_scope("events.subscribe"), "read");
        assert_eq!(required_scope("automation.create"), "orchestration");
        assert_eq!(required_scope("automation.rebind"), "orchestration");
        assert!(!is_read_only("automation.create"));
        assert!(!is_read_only("automation.rebind"));
        assert!(is_idempotent("automation.rebind"));
        assert!(is_read_only("automation.preview"));
        for stream in [
            "events.subscribe",
            "terminal.backend.events.subscribe",
            "terminal.backend.observe",
        ] {
            assert!(is_read_only(stream));
            assert!(!is_idempotent(stream));
        }
        assert_eq!(capabilities["protocol"]["name"], "luvus-uhp");
        assert!(capabilities.get("profiles").is_none());
    }
}
