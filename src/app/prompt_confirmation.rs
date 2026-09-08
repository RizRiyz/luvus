//! Prompt identity confirmation, before the existing until-wait workflow.
use super::*;
use crate::terminal::vt::PromptInputRegion;

pub(super) struct Echo {
    pub text: String,
    pub before: Option<PromptInputRegion>,
    pub deadline: Instant,
    pub wait: bool,
}

pub(super) fn matches(region: &PromptInputRegion, prompt: &str) -> bool {
    let first = prompt.split('\n').next().unwrap_or(prompt);
    let visible = region.text.as_str();
    !visible.is_empty()
        && (visible == prompt
            || visible == first
            || (unicode_width::UnicodeWidthStr::width(visible) == region.capacity
                && (prompt.starts_with(visible) || first.starts_with(visible))))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn failure(
    request_id: &str,
    pane: PaneId,
    code: &str,
    message: &str,
    queued: bool,
    submitted: bool,
    submission: &str,
    baseline_revision: u64,
    content_revision: u64,
) -> String {
    json!({"id":request_id,"error":{
        "code":code,"message":message,"data":{
            "pane":pane.0.to_string(),"queued":queued,"submitted":submitted,
            "submission":submission,"reason":code,
            "baseline_revision":baseline_revision,"content_revision":content_revision
        }
    }})
    .to_string()
}

impl AgentPrompt {
    pub(super) fn failure(&self, pane: PaneId, code: &str, message: &str) -> String {
        failure(
            &self.request_id,
            pane,
            code,
            message,
            true,
            self.echo.is_none(),
            if self.echo.is_some() {
                "failed"
            } else {
                self.submission
            },
            self.baseline_revision,
            self.last_revision,
        )
    }
}
