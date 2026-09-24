//! Commander composer, exact-target syntax, and App-owned delivery.
//! Only input/render integration remains at the existing app and UI boundaries.

mod app;
mod composer;
mod targets;

pub(crate) use composer::{line_end, line_start, next_word, previous_word, Commander};
pub(crate) use targets::{
    encode_component, parse_scoped_target, target_lookup, target_spans, unescape_pane_mentions,
    DeliveryPlan, ExactTarget, ScopedTarget, MAX_TARGETS,
};
