//! Identity. `PaneId` is a process-global monotonic counter so a pane keeps its
//! id across splits and moves. (Public base-32 ids land with the data model.)

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PaneId(pub u32);

static NEXT_PANE_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_PUBLIC_ID: AtomicU64 = AtomicU64::new(1);

/// Opaque public identity for persisted topology objects. The OS-random path
/// is effectively collision-free; the process/counter fallback keeps object
/// creation available even if system randomness is temporarily unavailable.
pub fn public_id(kind: &str) -> String {
    let body = crate::terminal::backend::random_id().unwrap_or_else(|_| {
        format!(
            "{:x}{:x}",
            std::process::id(),
            NEXT_PUBLIC_ID.fetch_add(1, Ordering::Relaxed)
        )
    });
    format!("{kind}_{body}")
}

impl PaneId {
    pub fn alloc() -> Self {
        PaneId(NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed))
    }
}
