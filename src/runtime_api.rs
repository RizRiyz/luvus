//! Versioned app-runtime API contract for orchestrators.

use serde_json::{json, Value};

pub const PROTOCOL_NAME: &str = "luvus-runtime";
pub const PROTOCOL_MAJOR: u64 = 1;
pub const PROTOCOL_MINOR: u64 = 0;

pub const METHODS: &[&str] = &[
    "runtime.capabilities",
    "session.snapshot",
    "pane.processes",
    "agent.explain",
    "agent.report",
    "agent.release",
    "agent.start",
    "agent.prompt",
    "agent.wait",
    "events.subscribe",
];

pub fn schema_bundle() -> Value {
    fn schema(source: &str) -> Value {
        serde_json::from_str(source).expect("embedded runtime API schema is valid JSON")
    }
    json!({
        "protocol":{"name":PROTOCOL_NAME,"major":PROTOCOL_MAJOR,"minor":PROTOCOL_MINOR},
        "request":schema(include_str!("../protocol/runtime/v1/schema/request.schema.json")),
        "response":schema(include_str!("../protocol/runtime/v1/schema/response.schema.json")),
        "event":schema(include_str!("../protocol/runtime/v1/schema/event.schema.json")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_runtime_contract_tracks_embedded_version_and_methods() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("protocol/runtime/v1");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(root.join("fixtures/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["protocol"]["name"], PROTOCOL_NAME);
        assert_eq!(manifest["protocol"]["major"], PROTOCOL_MAJOR);
        assert_eq!(manifest["protocol"]["minor"], PROTOCOL_MINOR);
        let bundle = schema_bundle();
        assert_eq!(
            bundle["request"]["oneOf"].as_array().unwrap().len(),
            METHODS.len()
        );
        let requests = std::fs::read_to_string(root.join("fixtures/valid/requests.jsonl")).unwrap();
        let fixture_methods: std::collections::BTreeSet<_> = requests
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["method"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let runtime_methods: std::collections::BTreeSet<_> =
            METHODS.iter().map(|method| method.to_string()).collect();
        assert_eq!(fixture_methods, runtime_methods);
        for fixture in manifest["files"].as_array().unwrap() {
            let lines = std::fs::read_to_string(
                root.join("fixtures")
                    .join(fixture["path"].as_str().unwrap()),
            )
            .unwrap();
            assert_eq!(
                lines.lines().count() as u64,
                fixture["count"].as_u64().unwrap()
            );
            assert!(lines
                .lines()
                .all(|line| serde_json::from_str::<Value>(line).is_ok()));
        }
    }
}
