//! Client-modal restoration bookkeeping, separate from endpoint switch intent.
use super::{Endpoint, SurfaceCandidate};

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Preparing(u64),
    /// Input was reclaimed, but queued ordinary frames remain untrusted.
    Failed,
}

#[derive(Default)]
pub(super) struct ModalRestore {
    phase: Phase,
    viewport: Option<(u16, u16)>,
}

impl ModalRestore {
    pub fn ticket_is(&self, ticket: u64) -> bool {
        matches!(self.phase, Phase::Preparing(current) if current == ticket)
    }
    pub fn blocked(&self) -> bool {
        matches!(self.phase, Phase::Failed)
    }
    pub fn viewport(&self) -> Option<(u16, u16)> {
        self.viewport
    }
    pub fn note_viewport(&mut self, size: (u16, u16)) -> bool {
        let changed = self.viewport != Some(size);
        self.viewport = Some(size);
        changed
    }
    pub fn owns(&self, candidate: Option<&SurfaceCandidate>, active: &Endpoint) -> bool {
        candidate
            .is_some_and(|pending| pending.endpoint == *active && self.ticket_is(pending.ticket))
    }
    pub fn handles_resize(&self, candidate: Option<&SurfaceCandidate>, active: &Endpoint) -> bool {
        (self.blocked() && candidate.is_none_or(|pending| pending.endpoint == *active))
            || self.owns(candidate, active)
    }
    pub fn can_start(&self, candidate: Option<&SurfaceCandidate>) -> bool {
        candidate.is_none_or(|pending| self.ticket_is(pending.ticket))
    }
    pub fn begin(&mut self, ticket: u64, size: (u16, u16)) {
        self.phase = Phase::Preparing(ticket);
        self.viewport = Some(size);
    }
    pub fn complete(&mut self) {
        self.phase = Phase::Idle;
    }
    pub fn cancel(&mut self, same_endpoint: bool) {
        if same_endpoint || self.blocked() {
            self.phase = Phase::Failed;
        } else {
            self.phase = Phase::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn modal_phases_preserve_candidate_ownership_and_visual_fences() {
        let active = Endpoint::Local;
        let mut pending = SurfaceCandidate {
            ticket: 7,
            endpoint: active.clone(),
            welcomed: true,
            ready: true,
            shell_dock: None,
            boot_id: None,
            deadline: Instant::now(),
        };
        let mut state = ModalRestore::default();
        assert!(!state.blocked());
        assert!(state.can_start(None));
        assert!(!state.can_start(Some(&pending)));
        state.begin(7, (80, 24));
        assert!(state.owns(Some(&pending), &active));
        assert!(state.handles_resize(Some(&pending), &active));
        assert!(!state.note_viewport((80, 24)));
        assert!(state.note_viewport((60, 20)));
        assert!(state.ticket_is(7));
        pending.endpoint = Endpoint::Remote {
            machine_id: "chosen".into(),
            session: "review".into(),
        };
        assert!(!state.owns(Some(&pending), &active));
        state.cancel(true);
        assert!(state.blocked());
        assert!(!state.ticket_is(7));
        assert!(state.handles_resize(None, &active));
        assert!(!state.handles_resize(Some(&pending), &active));
        state.cancel(false);
        assert!(
            state.blocked(),
            "switch failure cannot clear an existing visual fence"
        );
        state.complete();
        assert!(!state.blocked());
        assert_eq!(state.viewport(), Some((60, 20)));
    }
}
