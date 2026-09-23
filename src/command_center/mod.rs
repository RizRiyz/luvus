//! Command Center composer, exact-target syntax, and App-owned delivery.
//! Only input/render integration remains at the existing app and UI boundaries.

mod app;
mod composer;
mod targets;

pub(crate) use composer::{line_end, line_start, next_word, previous_word, CommandCenter};
pub(crate) use targets::{
    target_lookup, target_spans, unescape_pane_mentions, DeliveryPlan, ExactTarget, MAX_TARGETS,
};
