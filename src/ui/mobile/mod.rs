//! Automatic mobile presentation for narrow terminal clients (docs/100).
//!
//! This module owns presentation and hit geometry only. Workspace mutations,
//! filtering, target activation, persistence, and IO remain in `src/app`.

mod header;
mod layout;
mod navigator;
pub(super) mod sheets;

pub(super) use header::render_header;
pub(super) use layout::{compute_layout, resolve_profile, MobileLayout, MobileProfile};
pub(super) use navigator::render_navigator;

#[cfg(test)]
mod tests;
