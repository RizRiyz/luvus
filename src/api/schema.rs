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
