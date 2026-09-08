//! Core JSON API handlers.

use super::*;

impl App {
    pub(super) fn api_ping(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        Ok(json!({
            "type":"pong",
            "version": env!("CARGO_PKG_VERSION"),
            "protocol":1,
            "session": crate::session::display_name()
        }))
    }

    pub(super) fn api_uhp_capabilities(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            let mut capabilities =
                crate::api::capabilities(crate::ipc::api::current_sequence(&self.events));
            if let Some(object) = capabilities.as_object_mut() {
                object.insert("session".into(), json!(crate::session::display_name()));
                object.insert(
                    "server_generation".into(),
                    json!(self.backend_server_generation),
                );
            }
            Ok(capabilities)
        }
    }

    pub(super) fn api_config_get(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            Ok(json!({"type":"config", "config":self.config}))
        }
    }

    pub(super) fn api_config_patch(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &["patch"])?;
            let patch = p.get("patch").ok_or_else(|| {
                (
                    "invalid_request".to_string(),
                    "config.patch needs a patch object".to_string(),
                )
            })?;
            let next = patched_config(&self.config, patch)?;
            self.apply_socket_config(next, Some(patch))?;
            Ok(json!({"type":"config", "config":self.config}))
        }
    }

    pub(super) fn api_server_reload_config(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            let next = crate::config::load();
            self.apply_socket_config(next, None)?;
            Ok(json!({"type":"config_reloaded", "config":self.config}))
        }
    }

    pub(super) fn api_server_agent_manifests(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            Ok(json!({
                "type":"agent_manifests",
                "rules":self.manifests.rule_count(),
                "agents":self.manifests.agent_names(),
            }))
        }
    }

    pub(super) fn api_server_reload_agent_manifests(
        &mut self,
        method: &str,
        p: &Value,
    ) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            let manifests = crate::detect::Manifests::load(&crate::persist::ensure_manifests_dir());
            self.apply_socket_manifests(manifests);
            let rules = self.manifests.rule_count();
            Ok(json!({"type":"agent_manifests_reloaded","rules":rules}))
        }
    }

    pub(super) fn api_session_snapshot(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            reject_api_fields(p, &[])?;
            Ok(self.runtime_snapshot())
        }
    }

    pub(super) fn api_server_stop(&mut self, method: &str, p: &Value) -> DispatchResult {
        let _ = (method, p);
        {
            self.should_quit = true;
            Ok(json!({"type":"ok"}))
        }
    }
}
