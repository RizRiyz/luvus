//! Identity. `PaneId` is a process-global monotonic counter so a pane keeps its
//! id across splits and moves. (Public base-32 ids land with the data model.)

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PaneId(pub u32);

static NEXT_PANE_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_PUBLIC_ID: AtomicU64 = AtomicU64::new(1);

/// Opaque public identity for persisted topology objects. The OS-random path
/// is effectively collision-free; the process/counter fallback keeps object
/// creation available even if system randomness is temporarily unavailable.
pub fn public_id(kind: &str) -> String {
    let body = crate::terminal::backend::random_id().unwrap_or_else(|_| fallback_public_id());
    format!("{kind}_{body}")
}

fn fallback_public_id() -> String {
    let pid = std::process::id();
    let marker = crate::platform::process_start_marker(pid).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string()
    });
    fallback_public_id_from(pid, &marker, NEXT_PUBLIC_ID.fetch_add(1, Ordering::Relaxed))
}

fn fallback_public_id_from(pid: u32, marker: &str, sequence: u64) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pid.hash(&mut hasher);
    marker.hash(&mut hasher);
    let lifetime = hasher.finish();
    format!("{lifetime:016x}{sequence:016x}")
}

impl PaneId {
    pub fn alloc() -> Self {
        PaneId(NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_public_ids_include_process_lifetime_and_sequence() {
        let first = fallback_public_id_from(7, "start-a", 1);
        let next = fallback_public_id_from(7, "start-a", 2);
        let restarted = fallback_public_id_from(7, "start-b", 1);
        assert_eq!(first.len(), 32);
        assert_ne!(first, next);
        assert_ne!(first, restarted);
    }
}
