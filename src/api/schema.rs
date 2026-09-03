use serde_json::{json, Value};

pub fn schema_bundle() -> Value {
    fn schema(source: &str) -> Value {
        serde_json::from_str(source).expect("embedded UHP schema is valid JSON")
    }
    let request = schema(include_str!(
        "../../protocol/uhp/v1/schema/request.schema.json"
    ));
    let response = schema(include_str!(
        "../../protocol/uhp/v1/schema/response.schema.json"
    ));
    let event = schema(include_str!(
        "../../protocol/uhp/v1/schema/event.schema.json"
    ));
    let event_catalog = schema(include_str!(
        "../../protocol/uhp/v1/schema/event-catalog.schema.json"
    ));
    let access_descriptor = schema(include_str!(
        "../../protocol/uhp/v1/schema/access/descriptor.schema.json"
    ));
    let terminal = crate::terminal::backend::schema_bundle();
    let mut documents = terminal["documents"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    documents.insert(
        "https://luvus.dev/protocol/uhp/v1/request.schema.json".into(),
        request.clone(),
    );
    documents.insert(
        "https://luvus.dev/protocol/uhp/v1/response.schema.json".into(),
        response.clone(),
    );
    documents.insert(
        "https://luvus.dev/protocol/uhp/v1/event.schema.json".into(),
        event.clone(),
    );
    documents.insert(
        "https://luvus.dev/protocol/uhp/v1/event-catalog.schema.json".into(),
        event_catalog.clone(),
    );
    documents.insert(
        "https://luvus.dev/protocol/uhp/v1/schema/access/descriptor.schema.json".into(),
        access_descriptor.clone(),
    );
    json!({
        "protocol":{
            "name":super::PROTOCOL_NAME,
            "major":super::PROTOCOL_MAJOR,
            "minor":super::PROTOCOL_MINOR,
        },
        "request":request,
        "response":response,
        "event":event,
        "event_catalog":event_catalog,
        "access":{"descriptor":access_descriptor},
        "terminal":terminal,
        "documents":documents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_method_enum_tracks_registry() {
        let bundle = schema_bundle();
        let methods = bundle["request"]["properties"]["method"]["enum"]
            .as_array()
            .unwrap();
        let schema: std::collections::BTreeSet<_> =
            methods.iter().map(|v| v.as_str().unwrap()).collect();
        let registry: std::collections::BTreeSet<_> =
            super::super::capabilities::all_methods().collect();
        assert_eq!(schema, registry);
    }

    #[test]
    fn schema_bundle_publishes_one_uhp_contract_with_terminal_components() {
        let bundle = schema_bundle();
        assert_eq!(bundle["protocol"]["name"], "luvus-uhp");
        assert_eq!(bundle["protocol"]["major"], 1);
        assert!(bundle.get("profiles").is_none());
        assert!(bundle["terminal"]["methods"]["observe"].is_object());
        assert!(bundle["terminal"]["methods"]["control"].is_object());
        assert_eq!(bundle["access"]["descriptor"]["type"], "object");
        let documents = bundle["documents"].as_object().unwrap();
        assert!(documents.contains_key(
            "https://luvus.dev/protocol/uhp/v1/schema/access/descriptor.schema.json"
        ));
        for branch in bundle["request"]["allOf"].as_array().unwrap() {
            let Some(reference) = branch["then"]["properties"]["params"]["$ref"].as_str() else {
                continue;
            };
            if reference.starts_with("https://") {
                assert!(
                    documents.contains_key(reference),
                    "missing schema {reference}"
                );
            }
        }
    }

    #[test]
    fn schema_bundle_publishes_the_general_event_catalog() {
        let bundle = schema_bundle();
        let catalog = bundle["event_catalog"]["properties"].as_object().unwrap();
        let actual: std::collections::BTreeSet<_> = catalog.keys().map(String::as_str).collect();
        let declared: std::collections::BTreeSet<_> = bundle["event_catalog"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        let expected: std::collections::BTreeSet<_> = [
            "agent.authority_released",
            "agent.authority_reported",
            "agent.hook",
            "automation.created",
            "automation.deleted",
            "automation.disabled",
            "automation.enabled",
            "automation.run_failed",
            "automation.run_finished",
            "automation.run_materialized",
            "automation.run_queued",
            "automation.run_started",
            "automation.run_updated",
            "automation.updated",
            "config.changed",
            "events.resync_required",
            "layout.applied",
            "layout.ratio_changed",
            "lease.acquired",
            "lease.released",
            "pane.agent_status_changed",
            "pane.closed",
            "pane.created",
            "pane.focused",
            "pane.forked",
            "pane.moved",
            "pane.renamed",
            "pane.resized",
            "pane.swapped",
            "pane.zoomed",
            "server.agent_manifests_reloaded",
            "tab.closed",
            "tab.created",
            "tab.moved",
            "task.added",
            "task.claimed",
            "task.deleted",
            "task.done",
            "task.gate_failed",
            "task.gate_passed",
            "task.gate_running",
            "task.merge_conflict",
            "task.merge_failed",
            "task.merge_started",
            "task.merged",
            "task.needs_compaction",
            "task.ready",
            "task.released",
            "task.started",
            "task.updated",
            "terminal.closed",
            "terminal.created",
            "terminal.exited",
            "terminal.metadata_changed",
            "terminal.moved",
            "terminal.output_ready",
            "workspace.block_moved",
            "workspace.closed",
            "workspace.created",
            "workspace.metadata_reported",
            "workspace.moved",
        ]
        .into_iter()
        .collect();
        assert_eq!(actual, expected);
        assert_eq!(declared, expected);

        let agent_status = &bundle["event_catalog"]["$defs"]["agent_status"];
        let required: std::collections::BTreeSet<_> = agent_status["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            required,
            [
                "agent",
                "authority",
                "branch",
                "cwd",
                "pane",
                "project",
                "status",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(agent_status["additionalProperties"], false);
        assert_eq!(
            catalog["pane.agent_status_changed"]["$ref"],
            "#/$defs/agent_status"
        );

        let automation_definition = &bundle["event_catalog"]["$defs"]["automation_definition"];
        assert_eq!(automation_definition["additionalProperties"], false);
        assert_eq!(
            automation_definition["properties"]["task"]["additionalProperties"],
            false
        );
        assert!(
            automation_definition["properties"]["task"]["properties"]
                .get("prompt")
                .is_none(),
            "automation events must not expose stored prompts"
        );
        assert_eq!(
            bundle["event_catalog"]["$defs"]["automation_run"]["additionalProperties"],
            false
        );

        fn assert_refs_resolve(root: &Value, documents: &Value, value: &Value) {
            if let Some(reference) = value.get("$ref").and_then(Value::as_str) {
                let (document, pointer) = reference.split_once('#').unwrap_or((reference, ""));
                let target = if document.is_empty() {
                    root
                } else {
                    documents
                        .get(document)
                        .unwrap_or_else(|| panic!("missing event catalog document {document}"))
                };
                assert!(
                    target.pointer(pointer).is_some(),
                    "missing event catalog reference {reference}"
                );
            }
            match value {
                Value::Array(values) => {
                    for value in values {
                        assert_refs_resolve(root, documents, value);
                    }
                }
                Value::Object(values) => {
                    for value in values.values() {
                        assert_refs_resolve(root, documents, value);
                    }
                }
                _ => {}
            }
        }
        assert_refs_resolve(
            &bundle["event_catalog"],
            &bundle["documents"],
            &bundle["event_catalog"],
        );

        for (id, document) in bundle["documents"].as_object().unwrap() {
            if let Some(declared_id) = document.get("$id").and_then(Value::as_str) {
                assert_eq!(declared_id, id, "schema document key must match its $id");
            }
        }

        let terminal_ref = catalog["terminal.created"]["$ref"].as_str().unwrap();
        let (document, pointer) = terminal_ref.split_once('#').unwrap();
        assert!(bundle["documents"].get(document).is_some());
        let terminal_data = bundle["documents"][document].pointer(pointer).unwrap();
        assert_eq!(terminal_data["type"], "object");
    }

    /// The catalog mirrors shared-workspace task bindings and started payloads.
    #[test]
    fn general_event_catalog_tracks_task_worker_payloads() {
        let bundle = schema_bundle();
        let definitions = &bundle["event_catalog"]["$defs"];

        assert_eq!(
            definitions["agent_status"]["properties"]["branch"]["type"],
            json!(["string", "null"])
        );

        let task = &definitions["task"];
        let task_required: std::collections::BTreeSet<_> = task["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert!(!task_required.contains("mode"));
        assert!(!task_required.contains("workspace_worker"));
        assert_eq!(
            task["properties"]["mode"]["enum"],
            json!(["worktree", "workspace"])
        );
        assert_eq!(
            task["properties"]["workspace_worker"]["$ref"],
            "#/$defs/workspace_worker"
        );

        let workspace_worker = &definitions["workspace_worker"];
        let workspace_worker_required: std::collections::BTreeSet<_> = workspace_worker["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            workspace_worker_required,
            ["root", "tab_id", "workspace_id"].into_iter().collect()
        );
        assert_eq!(workspace_worker["additionalProperties"], false);
        assert_eq!(workspace_worker["properties"]["root"]["type"], "string");
        assert_eq!(workspace_worker["properties"]["tab_id"]["type"], "string");
        assert_eq!(
            workspace_worker["properties"]["workspace_id"]["type"],
            "string"
        );

        let task_started = &definitions["task_started"];
        let task_started_required: std::collections::BTreeSet<_> = task_started["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            task_started_required,
            [
                "branch",
                "cwd",
                "id",
                "mode",
                "pane",
                "tab_id",
                "workspace_id",
                "worktree",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(task_started["additionalProperties"], false);
        assert_eq!(
            task_started["properties"]["mode"]["enum"],
            json!(["worktree", "workspace"])
        );
        for field in ["workspace_id", "tab_id", "cwd"] {
            assert_eq!(task_started["properties"][field]["type"], "string");
        }
        for field in ["worktree", "branch"] {
            assert_eq!(
                task_started["properties"][field]["type"],
                json!(["string", "null"])
            );
        }
    }

    #[test]
    fn published_fixture_manifest_tracks_version_and_line_counts() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("protocol/uhp/v1");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(root.join("fixtures/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["protocol"]["name"], super::super::PROTOCOL_NAME);
        assert_eq!(manifest["protocol"]["major"], super::super::PROTOCOL_MAJOR);
        assert_eq!(manifest["protocol"]["minor"], super::super::PROTOCOL_MINOR);
        for fixture in manifest["files"].as_array().unwrap() {
            let content = std::fs::read_to_string(
                root.join("fixtures")
                    .join(fixture["path"].as_str().unwrap()),
            )
            .unwrap();
            assert_eq!(
                content.lines().count() as u64,
                fixture["count"].as_u64().unwrap()
            );
            assert!(content
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok()));
        }
    }
}
