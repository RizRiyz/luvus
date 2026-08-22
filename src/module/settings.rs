//! Module settings values (docs/13 §3.6): the user-set half of a module's
//! `[[settings]]` declaration.
//!
//! The manifest declares the *shape* (key, title, type, bounds); this file
//! stores the *values*, as a flat JSON object in the module's own config dir
//! (`~/.luvus/modules/config/<id>/settings.json`). That dir already belongs to
//! the module and survives reinstalls, so a module can also read the file
//! directly if it would rather not parse the injected env.
//!
//! Only keys the manifest still declares are surfaced; a value left over from
//! an older manifest version stays on disk untouched but is ignored.

use serde_json::{Map, Value};

use super::manifest::ModuleManifest;
use super::paths;

pub const FILE: &str = "settings.json";

fn path(id: &str) -> std::path::PathBuf {
    paths::config_dir(id).join(FILE)
}

/// The raw stored values (missing/corrupt file reads as empty).
pub fn stored(id: &str) -> Map<String, Value> {
    std::fs::read_to_string(path(id))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// Manifest defaults overlaid with the user's stored values, restricted to the
/// keys the manifest still declares. This is what commands and the UI see.
pub fn effective(manifest: &ModuleManifest, id: &str) -> Map<String, Value> {
    let saved = stored(id);
    manifest
        .settings
        .iter()
        .map(|spec| {
            let v = saved
                .get(&spec.key)
                .cloned()
                .unwrap_or_else(|| spec.default_value());
            (spec.key.clone(), coerce(spec, v))
        })
        .collect()
}

/// One effective value.
pub fn get(manifest: &ModuleManifest, id: &str, key: &str) -> Option<Value> {
    let spec = manifest.setting(key)?;
    let v = stored(id)
        .get(key)
        .cloned()
        .unwrap_or_else(|| spec.default_value());
    Some(coerce(spec, v))
}

/// Validate `value` against the declared spec and persist it. Returns the value
/// as stored (coerced + clamped).
pub fn set(manifest: &ModuleManifest, id: &str, key: &str, value: Value) -> Result<Value, String> {
    let spec = manifest
        .setting(key)
        .ok_or_else(|| format!("module {id} has no setting {key}"))?;
    let value = validate(spec, value)?;
    let mut all = stored(id);
    all.insert(key.to_string(), value.clone());
    let dir = paths::config_dir(id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(&Value::Object(all))
        .map_err(|e| format!("cannot encode settings: {e}"))?;
    // Write via a temp file + rename so a crash mid-write can't truncate the
    // module's settings (same discipline as the module registry).
    let tmp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path(id)).map_err(|e| format!("cannot save settings: {e}"))?;
    Ok(value)
}

/// Reject a value of the wrong type; clamp numbers into `min..=max` and refuse
/// an enum choice that isn't offered.
fn validate(spec: &super::manifest::SettingSpec, v: Value) -> Result<Value, String> {
    use super::manifest::SettingKind as K;
    match spec.kind {
        K::Bool => match v {
            Value::Bool(b) => Ok(Value::Bool(b)),
            Value::String(s) if s == "true" || s == "false" => Ok(Value::Bool(s == "true")),
            _ => Err(format!("setting {} expects true or false", spec.key)),
        },
        K::Number => {
            let n = match &v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            }
            .ok_or_else(|| format!("setting {} expects a whole number", spec.key))?;
            Ok(Value::from(clamp(spec, n)))
        }
        K::Enum => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("setting {} expects one of its options", spec.key))?;
            if !spec.options.iter().any(|o| o == s) {
                return Err(format!(
                    "setting {} must be one of: {}",
                    spec.key,
                    spec.options.join(", ")
                ));
            }
            Ok(Value::String(s.to_string()))
        }
        K::String => match v {
            Value::String(s) => Ok(Value::String(s)),
            Value::Null => Ok(Value::String(String::new())),
            other => Ok(Value::String(other.to_string())),
        },
    }
}

/// Best-effort repair of an on-disk value whose type drifted from the manifest
/// (a module can change a setting's type between versions). Never fails — it
/// falls back to the declared default.
fn coerce(spec: &super::manifest::SettingSpec, v: Value) -> Value {
    validate(spec, v).unwrap_or_else(|_| spec.default_value())
}

fn clamp(spec: &super::manifest::SettingSpec, n: i64) -> i64 {
    let n = spec.min.map_or(n, |lo| n.max(lo));
    spec.max.map_or(n, |hi| n.min(hi))
}

/// Step a number/bool/enum setting by `delta` (the `‹ ›` arrows in Settings).
/// Strings are not steppable and come back unchanged.
pub fn stepped(spec: &super::manifest::SettingSpec, current: &Value, delta: i64) -> Value {
    use super::manifest::SettingKind as K;
    match spec.kind {
        K::Bool => Value::Bool(!current.as_bool().unwrap_or(false)),
        K::Number => {
            let step = spec.step.unwrap_or(1).max(1);
            let now = current.as_i64().unwrap_or_else(|| spec.min.unwrap_or(0));
            Value::from(clamp(spec, now + delta * step))
        }
        K::Enum if !spec.options.is_empty() => {
            let n = spec.options.len() as i64;
            let at = current
                .as_str()
                .and_then(|s| spec.options.iter().position(|o| o == s))
                .unwrap_or(0) as i64;
            // Wrap in both directions so `‹` off the front lands on the last option.
            let next = ((at + delta) % n + n) % n;
            Value::String(spec.options[next as usize].clone())
        }
        _ => current.clone(),
    }
}

/// A one-line rendering of a value for the Settings row (secrets are masked).
pub fn display(spec: &super::manifest::SettingSpec, v: &Value) -> String {
    if spec.secret {
        let empty = v.as_str().is_some_and(|s| s.is_empty());
        return if empty {
            "not set".into()
        } else {
            "••••••".into()
        };
    }
    match v {
        Value::Bool(b) => if *b { "on" } else { "off" }.to_string(),
        Value::String(s) if s.is_empty() => "not set".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The env a module command sees for its settings: the whole set as JSON plus
/// one `LUVUS_SETTING_<KEY>` per entry, so a shell script needs no JSON parser.
pub fn env(manifest: &ModuleManifest, id: &str) -> Vec<(String, String)> {
    if manifest.settings.is_empty() {
        return Vec::new();
    }
    let values = effective(manifest, id);
    let mut env = vec![(
        "LUVUS_MODULE_SETTINGS_JSON".to_string(),
        Value::Object(values.clone()).to_string(),
    )];
    for (k, v) in &values {
        let name = format!(
            "LUVUS_SETTING_{}",
            k.to_uppercase().replace(['-', ':', '.'], "_")
        );
        // Scalars go in bare (no JSON quotes) so `$LUVUS_SETTING_TOKEN` is usable.
        let flat = match v {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            other => other.to_string(),
        };
        env.push((name, flat));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::manifest::{SettingKind, SettingSpec};
    use crate::persist::TEST_ENV_LOCK;

    fn spec(key: &str, kind: SettingKind) -> SettingSpec {
        SettingSpec {
            key: key.into(),
            title: key.into(),
            kind,
            default: None,
            options: vec![],
            min: None,
            max: None,
            step: None,
            secret: false,
        }
    }

    fn manifest(settings: Vec<SettingSpec>) -> ModuleManifest {
        ModuleManifest {
            id: "you.demo".into(),
            name: "Demo".into(),
            version: "0.1.0".into(),
            min_luvus_version: "0.1.0".into(),
            description: None,
            platforms: None,
            build: vec![],
            startup: vec![],
            actions: vec![],
            events: vec![],
            panes: vec![],
            docks: vec![],
            bars: vec![],
            settings,
        }
    }

    #[test]
    fn set_get_roundtrip_with_clamping_and_env() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let mut limit = spec("limit", SettingKind::Number);
        limit.min = Some(1);
        limit.max = Some(10);
        let mut mode = spec("mode", SettingKind::Enum);
        mode.options = vec!["fast".into(), "slow".into()];
        let mut token = spec("token", SettingKind::String);
        token.secret = true;
        let m = manifest(vec![limit, mode, token, spec("loud", SettingKind::Bool)]);

        // Defaults before anything is set.
        assert_eq!(
            get(&m, "you.demo", "limit").unwrap(),
            1,
            "min is the default"
        );
        assert_eq!(get(&m, "you.demo", "mode").unwrap(), "fast");

        // Out-of-range numbers clamp rather than erroring.
        assert_eq!(set(&m, "you.demo", "limit", 99.into()).unwrap(), 10);
        assert_eq!(get(&m, "you.demo", "limit").unwrap(), 10);

        // A bad enum choice is refused, and the old value survives.
        assert!(set(&m, "you.demo", "mode", "sideways".into()).is_err());
        assert_eq!(get(&m, "you.demo", "mode").unwrap(), "fast");
        assert!(set(&m, "you.demo", "mode", "slow".into()).is_ok());

        // An unknown key is an error, not a silent write.
        assert!(set(&m, "you.demo", "nope", 1.into()).is_err());

        set(&m, "you.demo", "token", "s3cret".into()).unwrap();
        set(&m, "you.demo", "loud", true.into()).unwrap();

        // Env: whole-set JSON + flat per-key vars usable straight from a shell.
        let env = env(&m, "you.demo");
        let find = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(find("LUVUS_SETTING_LIMIT"), "10");
        assert_eq!(find("LUVUS_SETTING_MODE"), "slow");
        assert_eq!(find("LUVUS_SETTING_TOKEN"), "s3cret", "no JSON quoting");
        assert_eq!(find("LUVUS_SETTING_LOUD"), "true");
        assert!(find("LUVUS_MODULE_SETTINGS_JSON").contains("\"mode\":\"slow\""));

        // Secrets are masked in the UI even though the env carries the value.
        let masked = display(m.setting("token").unwrap(), &"s3cret".into());
        assert_eq!(masked, "••••••");

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn stepping_wraps_enums_and_clamps_numbers() {
        let mut n = spec("n", SettingKind::Number);
        (n.min, n.max, n.step) = (Some(0), Some(4), Some(2));
        assert_eq!(stepped(&n, &0.into(), 1), 2);
        assert_eq!(stepped(&n, &4.into(), 1), 4, "clamped at max");
        assert_eq!(stepped(&n, &0.into(), -1), 0, "clamped at min");

        let mut e = spec("e", SettingKind::Enum);
        e.options = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(stepped(&e, &"c".into(), 1), "a", "wraps forward");
        assert_eq!(stepped(&e, &"a".into(), -1), "c", "wraps backward");

        let b = spec("b", SettingKind::Bool);
        assert_eq!(stepped(&b, &false.into(), 1), true);

        // Strings are not steppable — the arrows leave them alone.
        let s = spec("s", SettingKind::String);
        assert_eq!(stepped(&s, &"hi".into(), 1), "hi");
    }

    #[test]
    fn stale_values_from_an_older_manifest_are_ignored() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("luvus-modstale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("LUVUS_HOME", &home);

        let m = manifest(vec![spec("kept", SettingKind::String)]);
        set(&m, "you.demo", "kept", "yes".into()).unwrap();
        // Hand-write a key the manifest no longer declares, plus a type mismatch.
        let dir = paths::config_dir("you.demo");
        std::fs::write(
            dir.join(FILE),
            r#"{"kept": 42, "dropped": "old", "gone": true}"#,
        )
        .unwrap();

        let eff = effective(&m, "you.demo");
        assert_eq!(eff.len(), 1, "only declared keys surface");
        // The number coerces to the declared string type rather than blowing up.
        assert_eq!(eff.get("kept").unwrap(), "42");
        // The undeclared value stays on disk (a downgrade shouldn't lose it).
        assert!(stored("you.demo").contains_key("dropped"));

        std::env::remove_var("LUVUS_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
