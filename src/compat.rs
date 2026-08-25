//! Compatibility bridge for the Bohay -> Luvus transition.
//!
//! Luvus 0.11 writes only the new names, but accepts the 0.10 environment
//! contract and emits both contracts to child panes and modules. Keep the
//! compatibility policy here so it can be removed cleanly after the 0.11.x
//! migration window.

use std::ffi::OsString;

pub const LEGACY_PREFIX: &str = "BOHAY_";
pub const CURRENT_PREFIX: &str = "LUVUS_";

const INHERITED_KEYS: &[(&str, &str)] = &[
    ("LUVUS_ENV", "BOHAY_ENV"),
    ("LUVUS_HOME", "BOHAY_HOME"),
    ("LUVUS_PANE_ID", "BOHAY_PANE_ID"),
    ("LUVUS_SOCKET_PATH", "BOHAY_SOCKET_PATH"),
    ("LUVUS_SESSION", "BOHAY_SESSION"),
    ("LUVUS_SHELL", "BOHAY_SHELL"),
    ("LUVUS_MANIFEST_URL", "BOHAY_MANIFEST_URL"),
    ("LUVUS_UPDATE_MANIFEST", "BOHAY_UPDATE_MANIFEST"),
    ("LUVUS_COPILOT_DIR", "BOHAY_COPILOT_DIR"),
];

/// Promote legacy input variables only when the canonical variable is absent.
/// Canonical Luvus configuration always wins when both are present.
pub fn normalize_legacy_environment() {
    for &(current, legacy) in INHERITED_KEYS {
        if std::env::var_os(current).is_none() {
            if let Some(value) = std::env::var_os(legacy) {
                std::env::set_var(current, value);
            }
        }
    }
}

/// Expand canonical child-process variables with their 0.10 aliases.
pub fn with_legacy_aliases(env: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(env.len() * 2);
    for (key, value) in env {
        if let Some(suffix) = key.strip_prefix(CURRENT_PREFIX) {
            out.push((format!("{LEGACY_PREFIX}{suffix}"), value.clone()));
        }
        out.push((key, value));
    }
    out
}

pub fn inherited(current: &str, legacy: &str) -> Option<OsString> {
    std::env::var_os(current).or_else(|| std::env::var_os(legacy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_env_wins_and_legacy_is_a_fallback() {
        let key = format!("LUVUS_COMPAT_TEST_{}", std::process::id());
        let old = key.replacen("LUVUS_", "BOHAY_", 1);
        std::env::remove_var(&key);
        std::env::set_var(&old, "old");
        assert_eq!(
            inherited(&key, &old).as_deref(),
            Some(std::ffi::OsStr::new("old"))
        );
        std::env::set_var(&key, "new");
        assert_eq!(
            inherited(&key, &old).as_deref(),
            Some(std::ffi::OsStr::new("new"))
        );
        std::env::remove_var(key);
        std::env::remove_var(old);
    }

    #[test]
    fn child_env_gets_both_names() {
        let env = with_legacy_aliases(vec![("LUVUS_PANE_ID".into(), "7".into())]);
        assert!(env.contains(&("LUVUS_PANE_ID".into(), "7".into())));
        assert!(env.contains(&("BOHAY_PANE_ID".into(), "7".into())));
    }
}
