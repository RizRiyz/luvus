use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::super::SessionInfo;

const MAX_CACHE_BYTES: u64 = 256 * 1024;

pub(in crate::agent) fn base() -> PathBuf {
    super::super::home().join(".gemini").join("antigravity-cli")
}

fn cache_path(base: &Path) -> PathBuf {
    base.join("cache").join("last_conversations.json")
}

fn sessions(base: &Path) -> Vec<SessionInfo> {
    let path = cache_path(base);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) if metadata.len() <= MAX_CACHE_BYTES => metadata,
        _ => return Vec::new(),
    };
    let mut input = Vec::new();
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    if file
        .take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut input)
        .is_err()
        || input.len() as u64 > MAX_CACHE_BYTES
    {
        return Vec::new();
    }
    let Ok(entries) = serde_json::from_slice::<BTreeMap<String, String>>(&input) else {
        return Vec::new();
    };
    let updated = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    entries
        .into_iter()
        .filter(|(cwd, session_id)| {
            !cwd.is_empty() && super::super::resume_command(super::NAME, session_id).is_some()
        })
        .map(|(cwd, session_id)| SessionInfo {
            agent: super::NAME.to_string(),
            session_id,
            cwd: PathBuf::from(cwd),
            updated,
        })
        .collect()
}

pub(in crate::agent) fn list(base: &Path, cwd: &Path) -> Vec<String> {
    sessions(base)
        .into_iter()
        .filter(|session| crate::platform::same_path(&session.cwd, cwd))
        .map(|session| session.session_id)
        .collect()
}

pub(in crate::agent) fn latest(base: &Path, cwd: &Path) -> Option<String> {
    list(base, cwd).into_iter().next()
}

pub(in crate::agent) fn recent(base: &Path, limit: usize) -> Vec<SessionInfo> {
    sessions(base).into_iter().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn fixture(label: &str) -> PathBuf {
        let root = crate::persist::skills_dir().join(format!(
            "antigravity-sessions-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("cache")).unwrap();
        root
    }

    #[test]
    fn reads_the_documented_workspace_conversation_cache() {
        let root = fixture("valid");
        fs::write(
            cache_path(&root),
            r#"{
  "/workspace/alpha": "ec33ebf9-0cba-4100-8142-c61503f6c587",
  "/workspace/beta": "f9e8d7c6-b5a4-3210-fedc-ba9876543210"
}"#,
        )
        .unwrap();

        assert_eq!(
            latest(&root, Path::new("/workspace/alpha")).as_deref(),
            Some("ec33ebf9-0cba-4100-8142-c61503f6c587")
        );
        assert_eq!(list(&root, Path::new("/workspace/beta")).len(), 1);
        let recent = recent(&root, 1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].agent, super::super::NAME);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_malformed_oversized_and_unsafe_cache_data() {
        let root = fixture("invalid");
        let path = cache_path(&root);
        fs::write(&path, b"not json").unwrap();
        assert!(recent(&root, 8).is_empty());

        fs::write(&path, vec![b'x'; MAX_CACHE_BYTES as usize + 1]).unwrap();
        assert!(recent(&root, 8).is_empty());

        fs::write(&path, r#"{"/workspace/alpha":"bad id; exit 1"}"#).unwrap();
        assert!(recent(&root, 8).is_empty());

        let _ = fs::remove_dir_all(root);
    }
}
