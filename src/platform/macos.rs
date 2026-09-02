//! Narrow macOS host-input helpers.

/// `CGEventSourceStateID::combinedSessionState`.
const COMBINED_SESSION_STATE: i32 = 0;
/// `CGEventFlags::maskAlternate`, which represents either Option key.
const OPTION_FLAG: u64 = 1 << 19;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceFlagsState(state_id: i32) -> u64;
}

/// Read the current modifier state without installing an event tap, polling,
/// spawning a thread, or requesting Accessibility permission.
pub(super) fn option_modifier_pressed() -> bool {
    // SAFETY: `CGEventSourceFlagsState` is a process-safe value query. The
    // combined-session state id is a documented enum value and needs no owned
    // pointer or callback lifetime.
    unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) & OPTION_FLAG != 0 }
}
