//! On-demand UHP host profile.
//!
//! Session servers remain isolated state owners. Cross-session and host
//! lifecycle operations run in the short-lived `luvus uhp proxy` process, so
//! they remain available when a target server is stopped and add no resident
//! thread, daemon, or idle memory to Luvus.

use std::process::Command;

use serde_json::{json, Value};

pub const METHODS: &[&str] = &[
    "host.capabilities",
    "host.info",
    "host.doctor",
    "host.update.check",
    "host.update.install",
    "session.list",
    "session.status",
    "session.start",
    "session.stop",
    "session.restart",
    "session.delete",
    "skill.status",
    "skill.enable",
    "skill.disable",
    "integration.status",
    "integration.install",
    "integration.uninstall",
];

#[derive(Debug)]
struct HostError {
    code: &'static str,
    message: String,
}

impl HostError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub fn handles(method: &str) -> bool {
    METHODS.contains(&method)
}

/// Handle one host-profile request frame. `None` means the request belongs to
/// the selected session and should be forwarded to its server unchanged.
pub fn handle_frame(frame: &str) -> anyhow::Result<Option<String>> {
    let request: Value = serde_json::from_str(frame)?;
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !handles(method) {
        return Ok(None);
    }
    let object = request
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("request must be an object"))?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "id" | "method" | "params" | "auth"))
    {
        return Err(anyhow::anyhow!("host request contains an unknown field"));
    }
    let id = request
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_request_id(id))
        .ok_or_else(|| anyhow::anyhow!("host request has an invalid id"))?;
    if request.get("auth").is_some() {
        let response = json!({"id":id,"error":{
            "code":"authorization_denied",
            "message":"session auth tokens cannot authorize host-profile methods"
        }});
        return Ok(Some(response.to_string()));
    }
    let params = request
        .get("params")
        .filter(|value| value.is_object())
        .ok_or_else(|| anyhow::anyhow!("host request params must be an object"))?;
    let response = match dispatch(method, params) {
        Ok(result) => json!({"id":id,"result":result}),
        Err(error) => json!({"id":id,"error":{"code":error.code,"message":error.message}}),
    };
    Ok(Some(response.to_string()))
}

fn dispatch(method: &str, params: &Value) -> Result<Value, HostError> {
    match method {
        "host.capabilities" => {
            reject_fields(params, &[])?;
            let contracts = METHODS
                .iter()
                .map(|method| {
                    json!({
                        "method":method,
                        "access":if crate::api::capabilities::is_read_only(method) { "read" } else { "write" },
                        "scope":crate::api::capabilities::required_scope(method),
                        "idempotent":crate::api::capabilities::is_read_only(method),
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({
                "type":"uhp_host_capabilities",
                "protocol":{"name":crate::api::PROTOCOL_NAME,"major":crate::api::PROTOCOL_MAJOR,"minor":crate::api::PROTOCOL_MINOR},
                "profile":"host",
                "transport":"stdio_proxy",
                "authority":"local_owner",
                "methods":METHODS,
                "method_contracts":contracts,
            }))
        }
        "host.info" => {
            reject_fields(params, &[])?;
            let sessions = crate::session::list_sessions()
                .map_err(|error| HostError::new("session_list_failed", error.to_string()))?;
            let running = sessions.iter().filter(|session| session.running).count();
            let executable = std::env::current_exe()
                .map_err(|error| HostError::new("host_info_failed", error.to_string()))?;
            Ok(json!({
                "type":"host_info",
                "version":env!("CARGO_PKG_VERSION"),
                "os":std::env::consts::OS,
                "arch":std::env::consts::ARCH,
                "executable":executable,
                "config_dir":crate::persist::config_dir(),
                "sessions":{"total":sessions.len(),"running":running},
            }))
        }
        "host.doctor" => {
            reject_fields(params, &[])?;
            Ok(host_doctor())
        }
        "host.update.check" => {
            reject_fields(params, &[])?;
            let status = crate::update::release_status()
                .map_err(|error| HostError::new("update_check_failed", error.to_string()))?;
            Ok(json!({
                "type":"host_update_status",
                "current":status.current,
                "latest":status.latest,
                "available":status.available,
            }))
        }
        "host.update.install" => {
            reject_fields(params, &["confirm"])?;
            require_confirmation(params, method)?;
            let result = crate::update::install_latest()
                .map_err(|error| HostError::new("update_install_failed", error.to_string()))?;
            Ok(json!({
                "type":"host_update_install",
                "current":result.current,
                "latest":result.latest,
                "channel":result.channel,
                "updated":result.updated,
            }))
        }
        "session.list" => {
            reject_fields(params, &[])?;
            let sessions = crate::session::list_sessions()
                .map_err(|error| HostError::new("session_list_failed", error.to_string()))?;
            Ok(json!({"type":"session_list","sessions":sessions}))
        }
        "session.status" => {
            reject_fields(params, &["name"])?;
            let target = target_name(params)?;
            Ok(
                json!({"type":"session_status","session":crate::session::session_info(target.as_deref())}),
            )
        }
        "session.start" | "session.stop" | "session.restart" => {
            reject_fields(params, &["name"])?;
            let target = target_name(params)?;
            let result = match method {
                "session.start" => crate::session::start_session(target.as_deref()),
                "session.stop" => crate::session::stop_session(target.as_deref()),
                "session.restart" => crate::session::restart_session(target.as_deref()),
                _ => unreachable!(),
            }
            .map_err(|message| HostError::new("session_lifecycle_failed", message))?;
            Ok(json!({"type":method.replace('.', "_"),"session":result}))
        }
        "session.delete" => {
            reject_fields(params, &["name", "confirm"])?;
            require_confirmation(params, method)?;
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| HostError::new("invalid_request", "name is required"))?;
            let target = crate::session::parse_target_name(name)
                .map_err(|message| HostError::new("invalid_session_name", message))?;
            let Some(name) = target else {
                return Err(HostError::new(
                    "invalid_session_name",
                    "deleting the default session is not supported",
                ));
            };
            let session = crate::session::delete_session(&name)
                .map_err(|message| HostError::new("session_delete_failed", message))?;
            Ok(json!({"type":"session_delete","session":session}))
        }
        "skill.status" => {
            reject_fields(params, &[])?;
            let destinations = crate::skill::status()
                .map_err(|error| HostError::new("skill_status_failed", error.to_string()))?
                .into_iter()
                .map(|status| {
                    json!({
                        "host":status.host.as_str(),
                        "target":status.target,
                        "state":status.state.as_str(),
                        "managed_release":status.managed_release,
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({
                "type":"skill_status",
                "bundled_release":crate::skill::bundled_release(),
                "destinations":destinations,
            }))
        }
        "skill.enable" | "skill.disable" => {
            reject_fields(params, &["confirm"])?;
            require_confirmation(params, method)?;
            let changes = if method == "skill.enable" {
                crate::skill::enable()
            } else {
                crate::skill::disable()
            }
            .map_err(|error| HostError::new("skill_change_failed", error.to_string()))?;
            let changes = changes
                .into_iter()
                .map(|change| {
                    json!({
                        "host":change.host.as_str(),
                        "target":change.target,
                        "action":change.action.as_str(),
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({"type":method.replace('.', "_"),"changes":changes}))
        }
        "integration.status" => {
            reject_fields(params, &[])?;
            let integrations = crate::integration::agent_ids()
                .map(|agent| {
                    json!({"agent":agent,"installed":crate::integration::is_installed(agent)})
                })
                .collect::<Vec<_>>();
            Ok(json!({"type":"integration_status","integrations":integrations}))
        }
        "integration.install" | "integration.uninstall" => {
            reject_fields(params, &["agent", "confirm"])?;
            require_confirmation(params, method)?;
            let agent = params
                .get("agent")
                .and_then(Value::as_str)
                .ok_or_else(|| HostError::new("invalid_request", "agent is required"))?;
            if !crate::integration::agent_ids().any(|candidate| candidate == agent) {
                return Err(HostError::new(
                    "unsupported_agent",
                    format!("no integration for {agent}"),
                ));
            }
            let result = if method == "integration.install" {
                crate::integration::install(agent)
            } else {
                crate::integration::uninstall(agent)
            };
            result
                .map_err(|error| HostError::new("integration_change_failed", error.to_string()))?;
            Ok(json!({
                "type":method.replace('.', "_"),
                "agent":agent,
                "installed":crate::integration::is_installed(agent),
            }))
        }
        _ => Err(HostError::new("unknown_method", "unknown host method")),
    }
}

fn require_confirmation(params: &Value, method: &str) -> Result<(), HostError> {
    if params.get("confirm").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(HostError::new(
            "confirmation_required",
            format!("{method} requires confirm:true"),
        ))
    }
}

fn host_doctor() -> Value {
    let tools = [
        ("git", &["--version"][..]),
        ("gh", &["--version"][..]),
        ("ssh", &["-V"][..]),
        ("curl", &["--version"][..]),
    ]
    .into_iter()
    .map(|(name, args)| probe_tool(name, args))
    .collect::<Vec<_>>();
    json!({
        "type":"host_doctor",
        "config_dir":crate::persist::config_dir(),
        "logs":{
            "path":crate::logging::resolved_dir(),
            "writable":crate::logging::log_dir_writable(),
        },
        "tools":tools,
    })
}

fn probe_tool(program: &str, args: &[&str]) -> Value {
    let output = crate::platform::no_window(Command::new(program).args(args)).output();
    let Ok(output) = output else {
        return json!({"name":program,"available":false});
    };
    let text = if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    let version = String::from_utf8_lossy(text)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.chars().take(160).collect::<String>());
    json!({
        "name":program,
        "available":output.status.success(),
        "version":version,
    })
}

fn target_name(params: &Value) -> Result<Option<String>, HostError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError::new("invalid_request", "name is required"))?;
    crate::session::parse_target_name(name)
        .map_err(|message| HostError::new("invalid_session_name", message))
}

fn reject_fields(params: &Value, allowed: &[&str]) -> Result<(), HostError> {
    let object = params
        .as_object()
        .ok_or_else(|| HostError::new("invalid_request", "params must be an object"))?;
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(HostError::new(
            "invalid_request",
            format!("unknown parameter `{field}`"),
        ));
    }
    Ok(())
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        let frame = json!({"id":"host-test","method":method,"params":params}).to_string();
        serde_json::from_str(&handle_frame(&frame).unwrap().unwrap()).unwrap()
    }

    #[test]
    fn host_profile_lists_and_deletes_only_confirmed_named_sessions() {
        let _env = crate::persist::test_env("uhp-host-sessions");
        let named = crate::session::session_dir_for(Some("review"));
        std::fs::create_dir_all(&named).unwrap();

        let listed = call("session.list", json!({}));
        assert!(listed["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["name"] == "review"));

        let denied = call("session.delete", json!({"name":"review"}));
        assert_eq!(denied["error"]["code"], "confirmation_required");
        assert!(named.exists());

        let deleted = call("session.delete", json!({"name":"review","confirm":true}));
        assert_eq!(deleted["result"]["type"], "session_delete");
        assert!(!named.exists());
    }

    #[test]
    fn host_profile_rejects_session_tokens_and_preserves_session_forwarding() {
        let token = json!({
            "id":"host-test","method":"session.list","auth":"token","params":{}
        })
        .to_string();
        let response: Value =
            serde_json::from_str(&handle_frame(&token).unwrap().unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "authorization_denied");

        let session = json!({"id":"1","method":"workspace.list","params":{}}).to_string();
        assert!(handle_frame(&session).unwrap().is_none());
    }

    #[test]
    fn host_profile_reports_info_and_requires_confirmation_for_mutations() {
        let _env = crate::persist::test_env("uhp-host-management");

        let info = call("host.info", json!({}));
        assert_eq!(info["result"]["type"], "host_info");
        assert_eq!(info["result"]["version"], env!("CARGO_PKG_VERSION"));

        let skills = call("skill.status", json!({}));
        assert_eq!(skills["result"]["type"], "skill_status");
        assert!(skills["result"]["destinations"].is_array());

        for method in ["host.update.install", "skill.enable", "skill.disable"] {
            let response = call(method, json!({}));
            assert_eq!(response["error"]["code"], "confirmation_required");
        }
        for method in ["integration.install", "integration.uninstall"] {
            let response = call(method, json!({"agent":"claude"}));
            assert_eq!(response["error"]["code"], "confirmation_required");
        }

        let invalid = call(
            "integration.install",
            json!({"agent":"unknown","confirm":true}),
        );
        assert_eq!(invalid["error"]["code"], "unsupported_agent");
    }
}
