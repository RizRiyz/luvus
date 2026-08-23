//! Harness-neutral terminal backend protocol primitives.
//!
//! The application adapter lives in `app::backend`; this module owns the
//! versioned limits, opaque terminal identity, capture contract, and errors so
//! the wire protocol does not leak `App`'s UI model.

use serde_json::{json, Value};

pub const PROTOCOL_NAME: &str = "luvus-terminal-backend";
pub const PROTOCOL_MAJOR: u64 = 1;
pub const PROTOCOL_MINOR: u64 = 0;

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_INVENTORY_TERMINALS: usize = 1024;
pub const MAX_CAPTURE_LINES: usize = 300;
pub const MAX_CAPTURE_BYTES: usize = 512 * 1024;
pub const MAX_INPUT_BYTES: usize = 256 * 1024;
pub const MAX_TITLE_BYTES: usize = 256;
pub const MAX_NOTIFICATION_TITLE_BYTES: usize = 256;
pub const MAX_NOTIFICATION_BODY_BYTES: usize = 8 * 1024;
pub const MAX_COMMAND_ARGS: usize = 128;
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_ARG_BYTES: usize = 16 * 1024;
pub const MAX_CWD_BYTES: usize = 4 * 1024;

pub const CAPABILITIES: &[&str] = &[
    "inventory",
    "validate",
    "capture",
    "type_literal",
    "submit_text",
    "send_key",
    "set_title",
    "notify_terminal",
    "create_workspace",
    "create_sibling",
    "close",
    "snapshot",
    "events",
    "wait_change",
    "wait_output",
    "process_inspection",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMode {
    Visible,
    RecentUnwrapped,
    Detection,
}

impl CaptureMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "visible" => Some(Self::Visible),
            "recent_unwrapped" => Some(Self::RecentUnwrapped),
            "detection" => Some(Self::Detection),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::RecentUnwrapped => "recent_unwrapped",
            Self::Detection => "detection",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureResult {
    pub text: String,
    pub lines: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct RuntimeLocator {
    pub server_generation: String,
    pub terminal_id: String,
    pub pane_id: String,
}

#[derive(Clone, Debug)]
pub enum CreatePlacement {
    Workspace,
    Sibling(RuntimeLocator),
}

#[derive(Clone, Debug)]
pub struct CreateCommit {
    pub placement: CreatePlacement,
    pub focus: bool,
    pub label: Option<String>,
}

/// Metadata that exists only for a successfully started PTY lifetime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalRuntime {
    pub terminal_id: String,
    pub pid: u32,
    pub start_marker: Option<String>,
}

impl TerminalRuntime {
    pub fn new(pid: u32) -> Result<Self, String> {
        if pid == 0 {
            return Err("cannot identify a terminal without a root process".into());
        }
        Ok(Self {
            terminal_id: random_id()?,
            pid,
            start_marker: crate::platform::process_start_marker(pid),
        })
    }
}

/// Independent 128-bit, lowercase-hex identity from the operating-system RNG.
pub fn random_id() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("operating-system RNG failed: {error}"))?;
    let mut id = String::with_capacity(32);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(id)
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchEvidence {
    NotStarted,
    Rejected,
}

impl DispatchEvidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendError {
    pub code: &'static str,
    pub message: String,
    pub dispatch: Option<DispatchEvidence>,
    pub metadata: Option<Value>,
}

impl BackendError {
    pub fn read(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            dispatch: None,
            metadata: None,
        }
    }

    pub fn mutation(
        code: &'static str,
        message: impl Into<String>,
        dispatch: DispatchEvidence,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            dispatch: Some(dispatch),
            metadata: None,
        }
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn envelope(&self, id: &str) -> String {
        let mut error = serde_json::Map::new();
        error.insert("code".into(), json!(self.code));
        // Diagnostics are intentionally bounded and never include terminal or
        // request content.
        error.insert("message".into(), json!(bounded_diagnostic(&self.message)));
        if let Some(dispatch) = self.dispatch {
            error.insert("dispatch".into(), json!(dispatch.as_str()));
        }
        if let Some(metadata) = &self.metadata {
            error.insert("metadata".into(), metadata.clone());
        }
        json!({"id": id, "error": Value::Object(error)}).to_string()
    }
}

fn bounded_diagnostic(message: &str) -> String {
    message.chars().take(512).collect()
}

pub fn limits_json() -> Value {
    json!({
        "request_bytes": MAX_FRAME_BYTES,
        "response_bytes": MAX_FRAME_BYTES,
        "inventory_terminals": MAX_INVENTORY_TERMINALS,
        "capture_lines": MAX_CAPTURE_LINES,
        "capture_bytes": MAX_CAPTURE_BYTES,
        "input_bytes": MAX_INPUT_BYTES,
        "logical_keys_per_request": 1,
        "title_bytes": MAX_TITLE_BYTES,
        "notification_title_bytes": MAX_NOTIFICATION_TITLE_BYTES,
        "notification_body_bytes": MAX_NOTIFICATION_BODY_BYTES,
        "command_args": MAX_COMMAND_ARGS,
        "command_bytes": MAX_COMMAND_BYTES,
        "command_arg_bytes": MAX_COMMAND_ARG_BYTES,
        "cwd_bytes": MAX_CWD_BYTES,
        "event_queue": crate::ipc::api::event_queue_capacity(),
        "event_subscribers": crate::ipc::api::max_event_subscribers(),
        "wait_timeout_ms": 300000,
    })
}

/// Installed, self-contained protocol schema for adapters that cannot rely on
/// a source checkout. `include_str!` keeps introspection available in packaged
/// binaries and makes schema drift a compile/test failure.
pub fn schema_bundle() -> Value {
    fn schema(source: &str) -> Value {
        serde_json::from_str(source).expect("embedded terminal backend schema is valid JSON")
    }
    json!({
        "protocol":{"name":PROTOCOL_NAME,"major":PROTOCOL_MAJOR,"minor":PROTOCOL_MINOR},
        "request":schema(include_str!("../../../protocol/terminal-backend/v1/schema/request.schema.json")),
        "response":schema(include_str!("../../../protocol/terminal-backend/v1/schema/response.schema.json")),
        "event":schema(include_str!("../../../protocol/terminal-backend/v1/schema/event.schema.json")),
        "methods":{
            "capabilities":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/capabilities.schema.json")),
            "inventory":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/inventory.schema.json")),
            "snapshot":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/snapshot.schema.json")),
            "validate":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/validate.schema.json")),
            "processes":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/processes.schema.json")),
            "capture":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/capture.schema.json")),
            "type_literal":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/type-literal.schema.json")),
            "submit_text":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/submit-text.schema.json")),
            "send_key":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/send-key.schema.json")),
            "set_title":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/set-title.schema.json")),
            "notify":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/notify.schema.json")),
            "create":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/create.schema.json")),
            "close":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/close.schema.json")),
            "events":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/events.schema.json")),
            "wait_change":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/wait-change.schema.json")),
            "wait_output":schema(include_str!("../../../protocol/terminal-backend/v1/schema/methods/wait-output.schema.json")),
        }
    })
}

pub fn reject_unknown_fields(value: &Value, allowed: &[&str]) -> Result<(), BackendError> {
    let object = value
        .as_object()
        .ok_or_else(|| BackendError::read("invalid_params", "params must be an object"))?;
    if let Some(field) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(BackendError::read(
            "invalid_params",
            format!("unknown parameter: {field}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_match_the_public_shape() {
        let first = random_id().unwrap();
        let second = random_id().unwrap();
        assert!(valid_id(&first));
        assert!(valid_id(&second));
        assert_ne!(first, second);
    }

    #[test]
    fn mutation_errors_carry_dispatch_evidence() {
        let wire = BackendError::mutation(
            "stale_terminal",
            "terminal identity no longer exists",
            DispatchEvidence::Rejected,
        )
        .envelope("a");
        let value: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(value["error"]["dispatch"], "rejected");
    }

    #[test]
    fn published_contract_tracks_runtime_version_limits_and_capabilities() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("protocol/terminal-backend/v1");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(root.join("fixtures/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["protocol"]["name"], PROTOCOL_NAME);
        assert_eq!(manifest["protocol"]["major"], PROTOCOL_MAJOR);
        assert_eq!(manifest["protocol"]["minor"], PROTOCOL_MINOR);

        let request_schema: Value = serde_json::from_slice(
            &std::fs::read(root.join("schema/request.schema.json")).unwrap(),
        )
        .unwrap();
        let methods = request_schema["oneOf"].as_array().unwrap();
        assert_eq!(methods.len(), CAPABILITIES.len());

        let capabilities_fixture =
            std::fs::read_to_string(root.join("fixtures/valid/responses.jsonl"))
                .unwrap()
                .lines()
                .next()
                .map(str::to_string)
                .unwrap();
        let fixture: Value = serde_json::from_str(&capabilities_fixture).unwrap();
        assert_eq!(fixture["result"]["protocol"]["major"], PROTOCOL_MAJOR);
        assert_eq!(fixture["result"]["protocol"]["minor"], PROTOCOL_MINOR);
        assert_eq!(MAX_FRAME_BYTES, 1_048_576);
        assert_eq!(MAX_CAPTURE_BYTES, 524_288);
        assert_eq!(MAX_INPUT_BYTES, 262_144);
    }
}
