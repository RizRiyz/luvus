//! Durable agent scheduling with UTC occurrence keys, owned by the Luvus server.
//!
//! This module owns definitions, deadlines, occurrence deduplication, and run
//! history. It does not launch processes: due runs are handed to ORCH by the
//! single mutable `App` owner.

mod model;
mod persist;
mod schedule;
mod worker;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub use model::*;
use serde::{Deserialize, Serialize};
pub(crate) use worker::run as run_worker;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AutomationHealth {
    pub definitions: usize,
    pub enabled: usize,
    pub scheduled: usize,
    pub running: usize,
    pub review: usize,
    pub failed: usize,
    pub next_run_at: Option<u64>,
}

pub const AUTOMATION_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
pub struct AutomationState {
    #[serde(default = "current_format_version")]
    pub(crate) format_version: u32,
    pub automations: Vec<Automation>,
    pub runs: Vec<AutomationRun>,
    #[serde(default)]
    next_automation: u64,
    #[serde(default)]
    next_run: u64,
    #[serde(default)]
    idempotency: Vec<IdempotencyRecord>,
    #[serde(skip)]
    pub(crate) persist_path: Option<PathBuf>,
    /// In-memory nearest deadline. Keeps the server tick's ordinary path O(1).
    #[serde(skip)]
    next_wake_at: Option<u64>,
}

impl Default for AutomationState {
    fn default() -> Self {
        Self {
            format_version: AUTOMATION_FORMAT_VERSION,
            automations: Vec::new(),
            runs: Vec::new(),
            next_automation: 0,
            next_run: 0,
            idempotency: Vec::new(),
            persist_path: None,
            next_wake_at: None,
        }
    }
}

const fn current_format_version() -> u32 {
    AUTOMATION_FORMAT_VERSION
}

impl AutomationState {
    pub fn load() -> Self {
        #[cfg(test)]
        {
            Self::default()
        }
        #[cfg(not(test))]
        {
            Self::load_from(crate::persist::session_dir().join("automations.json"))
        }
    }

    fn load_from(path: PathBuf) -> Self {
        persist::load(path)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = self.persist_path.as_deref() else {
            return Ok(());
        };
        persist::save(self, path)
    }

    pub fn automation(&self, id: &str) -> Option<&Automation> {
        self.automations
            .iter()
            .find(|automation| automation.id == id)
    }

    pub fn run(&self, id: &str) -> Option<&AutomationRun> {
        self.runs.iter().find(|run| run.id == id)
    }

    pub fn create(
        &mut self,
        input: CreateAutomation,
        idempotency_key: Option<&str>,
        now: u64,
    ) -> Result<Automation, Reject> {
        let fingerprint = create_fingerprint(&input);
        if let Some(existing) =
            self.idempotent_result("automation.create", idempotency_key, &fingerprint)?
        {
            return self.automation(existing).cloned().ok_or_else(|| {
                Reject::new("idempotency_stale", "idempotent result no longer exists")
            });
        }
        validate_create(&input, now)?;
        if self.automations.len() >= MAX_AUTOMATIONS {
            return Err(Reject::new(
                "automation_limit",
                format!("at most {MAX_AUTOMATIONS} automations are allowed"),
            ));
        }
        self.next_automation += 1;
        let automation = Automation {
            id: format!("a{}", self.next_automation),
            name: input.name.trim().to_string(),
            enabled: input.enabled,
            next_run_at: input
                .enabled
                .then(|| schedule::first_at_or_after(&input.trigger, now))
                .flatten(),
            trigger: input.trigger,
            target: input.target,
            task: input.task,
            policy: input.policy,
            created_at: now,
            updated_at: now,
        };
        self.automations.push(automation.clone());
        self.remember_idempotency(
            "automation.create",
            idempotency_key,
            fingerprint,
            automation.id.clone(),
            now,
        );
        self.refresh_deadline();
        Ok(automation)
    }

    /// Resolve an already-completed create request before revalidating mutable
    /// external targets such as an open workspace. This lets a client safely
    /// retry after losing the response even if the workspace later closes.
    pub fn create_retry(
        &self,
        input: &CreateAutomation,
        idempotency_key: Option<&str>,
    ) -> Result<Option<Automation>, Reject> {
        let fingerprint = create_fingerprint(input);
        let Some(id) =
            self.idempotent_result("automation.create", idempotency_key, &fingerprint)?
        else {
            return Ok(None);
        };
        self.automation(id)
            .cloned()
            .map(Some)
            .ok_or_else(|| Reject::new("idempotency_stale", "idempotent result no longer exists"))
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool, now: u64) -> Result<Automation, Reject> {
        let index = self
            .automations
            .iter()
            .position(|automation| automation.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such automation: {id}")))?;
        let next_run_at = if enabled {
            Some(
                schedule::first_at_or_after(&self.automations[index].trigger, now).ok_or_else(
                    || {
                        Reject::new(
                            "schedule_expired",
                            "the schedule has no occurrence at or after the current UTC time",
                        )
                    },
                )?,
            )
        } else {
            None
        };
        let automation = &mut self.automations[index];
        automation.enabled = enabled;
        automation.updated_at = now;
        automation.next_run_at = next_run_at;
        let automation = automation.clone();
        if !enabled {
            for run in self
                .runs
                .iter_mut()
                .filter(|run| run.automation_id == id && run.status == RunStatus::Pending)
            {
                run.status = RunStatus::Cancelled;
                run.error = Some("automation disabled before launch".into());
                run.finished_at = Some(now);
            }
        }
        self.refresh_deadline();
        Ok(automation)
    }

    pub fn update(
        &mut self,
        id: &str,
        input: CreateAutomation,
        now: u64,
    ) -> Result<Automation, Reject> {
        validate_create(&input, now)?;
        let automation = self
            .automations
            .iter_mut()
            .find(|automation| automation.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such automation: {id}")))?;
        automation.name = input.name.trim().to_string();
        automation.enabled = input.enabled;
        automation.trigger = input.trigger;
        automation.target = input.target;
        automation.task = input.task;
        automation.policy = input.policy;
        automation.next_run_at = if automation.enabled {
            schedule::first_at_or_after(&automation.trigger, now)
        } else {
            None
        };
        automation.updated_at = now;
        let automation = automation.clone();
        self.refresh_deadline();
        Ok(automation)
    }

    pub fn delete(&mut self, id: &str) -> Result<Automation, Reject> {
        if self
            .runs
            .iter()
            .any(|run| run.automation_id == id && run.status.is_live())
        {
            return Err(Reject::new(
                "automation_active",
                "disable the automation and let its live run finish before deleting it",
            ));
        }
        let index = self
            .automations
            .iter()
            .position(|automation| automation.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such automation: {id}")))?;
        let automation = self.automations.remove(index);
        self.refresh_deadline();
        Ok(automation)
    }

    pub fn preview(trigger: &Trigger, now: u64, limit: usize) -> Result<Vec<u64>, Reject> {
        schedule::validate(trigger)?;
        Ok(schedule::preview(trigger, now, limit))
    }

    pub fn request_run(
        &mut self,
        automation_id: &str,
        idempotency_key: Option<&str>,
        now: u64,
    ) -> Result<AutomationRun, Reject> {
        let fingerprint = automation_id.to_string();
        if let Some(existing) =
            self.idempotent_result("automation.run", idempotency_key, &fingerprint)?
        {
            return self.run(existing).cloned().ok_or_else(|| {
                Reject::new("idempotency_stale", "idempotent result no longer exists")
            });
        }
        let automation = self.automation(automation_id).cloned().ok_or_else(|| {
            Reject::new("not_found", format!("no such automation: {automation_id}"))
        })?;
        if self.has_live_run(automation_id) {
            return Err(Reject::new(
                "automation_busy",
                "this automation already has a live run",
            ));
        }
        let run = self.push_run(&automation, now, now, RunStatus::Pending, None);
        self.remember_idempotency(
            "automation.run",
            idempotency_key,
            fingerprint,
            run.id.clone(),
            now,
        );
        Ok(run)
    }

    /// Resolve a previously accepted run request without emitting another
    /// queued event or attempting a second launch.
    pub fn run_retry(
        &self,
        automation_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<Option<AutomationRun>, Reject> {
        let Some(id) = self.idempotent_result("automation.run", idempotency_key, automation_id)?
        else {
            return Ok(None);
        };
        self.run(id)
            .cloned()
            .map(Some)
            .ok_or_else(|| Reject::new("idempotency_stale", "idempotent result no longer exists"))
    }

    /// Materialize at most one overdue occurrence per automation. This is O(n)
    /// only when the nearest cached deadline is due, never on every render.
    pub fn collect_due(&mut self, now: u64) -> Vec<AutomationRunId> {
        if self.next_wake_at.is_none_or(|deadline| deadline > now) {
            return Vec::new();
        }
        let due: Vec<(String, u64)> = self
            .automations
            .iter()
            .filter_map(|automation| {
                automation
                    .enabled
                    .then_some(automation.next_run_at)
                    .flatten()
                    .filter(|deadline| *deadline <= now)
                    .map(|deadline| (automation.id.clone(), deadline))
            })
            .collect();
        let mut created = Vec::new();
        for (automation_id, scheduled_at) in due {
            let Some(index) = self
                .automations
                .iter()
                .position(|automation| automation.id == automation_id)
            else {
                continue;
            };
            let automation = self.automations[index].clone();
            let occurrence_at = if automation.policy.misfire == MisfirePolicy::RunLatest {
                schedule::latest_at_or_before(&automation.trigger, now).unwrap_or(scheduled_at)
            } else {
                scheduled_at
            };
            let active = self.has_active_run(&automation_id);
            let queued = self.has_pending_run(&automation_id);
            let duplicate = self
                .runs
                .iter()
                .any(|run| run.automation_id == automation_id && run.scheduled_at == occurrence_at);
            let too_late =
                now.saturating_sub(occurrence_at) > automation.policy.misfire_grace_seconds;
            let status = if duplicate {
                None
            } else if (active || queued) && automation.policy.overlap == OverlapPolicy::Skip {
                Some((
                    RunStatus::Skipped,
                    Some("previous run is still active".to_string()),
                ))
            } else if too_late && automation.policy.misfire == MisfirePolicy::Skip {
                Some((
                    RunStatus::Skipped,
                    Some("occurrence exceeded its misfire grace".to_string()),
                ))
            } else if active && !queued {
                // QueueOne retains exactly one durable pending occurrence while
                // an earlier run owns the live ORCH task.
                Some((RunStatus::Pending, None))
            } else if active || queued {
                Some((
                    RunStatus::Skipped,
                    Some("one occurrence is already queued".to_string()),
                ))
            } else {
                Some((RunStatus::Pending, None))
            };
            if let Some((status, error)) = status {
                let run = self.push_run(&automation, occurrence_at, now, status, error);
                if run.status == RunStatus::Pending {
                    created.push(run.id);
                }
            }
            let next = if matches!(automation.trigger, Trigger::Once { .. }) {
                None
            } else {
                // Latest-only recovery: advance straight past `now`, never make
                // the server replay an unbounded backlog after sleep/restart.
                schedule::next_after(&automation.trigger, now)
            };
            let target = &mut self.automations[index];
            target.next_run_at = next;
            if next.is_none() {
                target.enabled = false;
            }
            target.updated_at = now;
        }
        self.refresh_deadline();
        created
    }

    pub fn pending_runs(&self) -> Vec<AutomationRunId> {
        let mut selected = Vec::new();
        for run in self
            .runs
            .iter()
            .filter(|run| run.status == RunStatus::Pending)
        {
            let blocked = self.runs.iter().any(|other| {
                other.id != run.id
                    && other.automation_id == run.automation_id
                    && matches!(
                        other.status,
                        RunStatus::Starting | RunStatus::Running | RunStatus::Review
                    )
            });
            if !blocked
                && !selected.iter().any(|id: &String| {
                    self.run(id)
                        .is_some_and(|chosen| chosen.automation_id == run.automation_id)
                })
            {
                selected.push(run.id.clone());
            }
        }
        selected
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.next_wake_at
    }

    pub fn latest_run(&self, automation_id: &str) -> Option<&AutomationRun> {
        self.runs
            .iter()
            .rev()
            .find(|run| run.automation_id == automation_id)
    }

    pub fn health(&self) -> AutomationHealth {
        let mut health = AutomationHealth {
            definitions: self.automations.len(),
            enabled: self.automations.iter().filter(|item| item.enabled).count(),
            scheduled: self
                .automations
                .iter()
                .filter(|item| item.enabled && item.next_run_at.is_some())
                .count(),
            next_run_at: self.next_deadline(),
            ..AutomationHealth::default()
        };
        for automation in &self.automations {
            match self.latest_run(&automation.id).map(|run| run.status) {
                Some(RunStatus::Pending | RunStatus::Starting | RunStatus::Running) => {
                    health.running += 1
                }
                Some(RunStatus::Review) => health.review += 1,
                Some(RunStatus::Failed) => health.failed += 1,
                _ => {}
            }
        }
        health
    }

    pub fn bind_task(&mut self, run_id: &str, task_id: String, now: u64) -> Result<(), Reject> {
        let run = self.run_mut(run_id)?;
        run.task_id = Some(task_id);
        run.status = RunStatus::Starting;
        run.started_at.get_or_insert(now);
        Ok(())
    }

    pub fn set_run_status(
        &mut self,
        run_id: &str,
        status: RunStatus,
        error: Option<String>,
        now: u64,
    ) -> Result<AutomationRun, Reject> {
        let run = self.run_mut(run_id)?;
        run.status = status;
        run.error = error.map(|value| truncate(value, MAX_ERROR_BYTES));
        if matches!(
            status,
            RunStatus::Starting | RunStatus::Running | RunStatus::Review
        ) {
            run.started_at.get_or_insert(now);
            run.finished_at = None;
        } else if !status.is_live() {
            run.finished_at = Some(now);
        }
        Ok(run.clone())
    }

    pub fn run_for_task_mut(&mut self, task_id: &str) -> Option<&mut AutomationRun> {
        self.runs
            .iter_mut()
            .find(|run| run.task_id.as_deref() == Some(task_id))
    }

    pub fn has_live_run(&self, automation_id: &str) -> bool {
        self.runs
            .iter()
            .any(|run| run.automation_id == automation_id && run.status.is_live())
    }

    fn has_active_run(&self, automation_id: &str) -> bool {
        self.runs.iter().any(|run| {
            run.automation_id == automation_id
                && matches!(
                    run.status,
                    RunStatus::Starting | RunStatus::Running | RunStatus::Review
                )
        })
    }

    fn has_pending_run(&self, automation_id: &str) -> bool {
        self.runs
            .iter()
            .any(|run| run.automation_id == automation_id && run.status == RunStatus::Pending)
    }

    fn run_mut(&mut self, id: &str) -> Result<&mut AutomationRun, Reject> {
        self.runs
            .iter_mut()
            .find(|run| run.id == id)
            .ok_or_else(|| Reject::new("not_found", format!("no such automation run: {id}")))
    }

    fn push_run(
        &mut self,
        automation: &Automation,
        scheduled_at: u64,
        now: u64,
        status: RunStatus,
        error: Option<String>,
    ) -> AutomationRun {
        self.next_run += 1;
        let run = AutomationRun {
            id: format!("r{}", self.next_run),
            automation_id: automation.id.clone(),
            scheduled_at,
            created_at: now,
            started_at: None,
            finished_at: (!status.is_live()).then_some(now),
            task_id: None,
            status,
            attempt: 1,
            error,
            trigger: Some(automation.trigger.clone()),
            policy: automation.policy.clone(),
            target: automation.target.clone(),
            task: automation.task.clone(),
        };
        self.runs.push(run.clone());
        if self.runs.len() > MAX_RUNS {
            let removable = self
                .runs
                .iter()
                .position(|candidate| !candidate.status.is_live());
            if let Some(index) = removable {
                self.runs.remove(index);
            }
        }
        run
    }

    fn idempotent_result<'a>(
        &'a self,
        operation: &str,
        key: Option<&str>,
        fingerprint: &str,
    ) -> Result<Option<&'a str>, Reject> {
        let Some(key) = key else { return Ok(None) };
        if key.is_empty() || key.len() > 128 {
            return Err(Reject::new(
                "invalid_idempotency_key",
                "idempotency_key must contain 1 to 128 bytes",
            ));
        }
        let Some(record) = self
            .idempotency
            .iter()
            .find(|record| record.key == key && record.operation == operation)
        else {
            return Ok(None);
        };
        if record.fingerprint != fingerprint {
            return Err(Reject::new(
                "idempotency_conflict",
                "idempotency_key was already used with different parameters",
            ));
        }
        Ok(Some(&record.result_id))
    }

    fn remember_idempotency(
        &mut self,
        operation: &str,
        key: Option<&str>,
        fingerprint: String,
        result_id: String,
        now: u64,
    ) {
        let Some(key) = key else { return };
        self.idempotency.push(IdempotencyRecord {
            key: key.to_string(),
            operation: operation.to_string(),
            fingerprint,
            result_id,
            created_at: now,
        });
        if self.idempotency.len() > MAX_IDEMPOTENCY_KEYS {
            let excess = self.idempotency.len() - MAX_IDEMPOTENCY_KEYS;
            self.idempotency.drain(..excess);
        }
    }

    pub(crate) fn normalize_after_load(&mut self) {
        if self.format_version == 0 {
            self.format_version = AUTOMATION_FORMAT_VERSION;
        }
        self.next_automation = self
            .next_automation
            .max(max_numeric_id(&self.automations, |item| &item.id));
        self.next_run = self
            .next_run
            .max(max_numeric_id(&self.runs, |item| &item.id));
        if self.runs.len() > MAX_RUNS {
            self.runs.drain(..self.runs.len() - MAX_RUNS);
        }
        if self.idempotency.len() > MAX_IDEMPOTENCY_KEYS {
            self.idempotency
                .drain(..self.idempotency.len() - MAX_IDEMPOTENCY_KEYS);
        }
        self.refresh_deadline();
    }

    pub(super) fn validate_loaded(&self) -> Result<(), Reject> {
        if self.automations.len() > MAX_AUTOMATIONS {
            return Err(Reject::new(
                "automation_limit",
                format!("at most {MAX_AUTOMATIONS} automations are allowed"),
            ));
        }

        let mut automation_ids = HashSet::with_capacity(self.automations.len());
        for automation in &self.automations {
            validate_identifier("automation.id", &automation.id)?;
            if !automation_ids.insert(automation.id.as_str()) {
                return Err(Reject::new(
                    "invalid_ledger",
                    format!("duplicate automation id: {}", automation.id),
                ));
            }
            validate_definition(
                &automation.name,
                &automation.trigger,
                &automation.target,
                &automation.task,
                &automation.policy,
            )?;
            if automation.enabled != automation.next_run_at.is_some() {
                return Err(Reject::new(
                    "invalid_ledger",
                    format!(
                        "automation {} has inconsistent enabled and next-run state",
                        automation.id
                    ),
                ));
            }
            if let Some(next_run_at) = automation.next_run_at {
                let valid_occurrence =
                    schedule::first_at_or_after(&automation.trigger, next_run_at)
                        == Some(next_run_at);
                if !valid_occurrence {
                    return Err(Reject::new(
                        "invalid_ledger",
                        format!(
                            "automation {} has a next run outside its schedule",
                            automation.id
                        ),
                    ));
                }
            }
        }

        let mut run_ids = HashSet::with_capacity(self.runs.len().min(MAX_RUNS));
        for run in &self.runs {
            validate_identifier("run.id", &run.id)?;
            validate_identifier("run.automation_id", &run.automation_id)?;
            if !run_ids.insert(run.id.as_str()) {
                return Err(Reject::new(
                    "invalid_ledger",
                    format!("duplicate automation run id: {}", run.id),
                ));
            }
            validate_task(&run.task)?;
            validate_target(&run.target)?;
            if let Some(trigger) = run.trigger.as_ref() {
                schedule::validate(trigger)?;
            }
            validate_policy(&run.policy)?;
            if run
                .error
                .as_ref()
                .is_some_and(|error| error.len() > MAX_ERROR_BYTES)
            {
                return Err(Reject::new(
                    "field_limit",
                    format!("run.error must be at most {MAX_ERROR_BYTES} bytes"),
                ));
            }
        }

        Ok(())
    }

    fn refresh_deadline(&mut self) {
        self.next_wake_at = self
            .automations
            .iter()
            .filter(|automation| automation.enabled)
            .filter_map(|automation| automation.next_run_at)
            .min();
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Resolve the server user's local IANA timezone once when an automation form
/// opens. Schedule evaluation remains UTC internally and performs no polling or
/// network access.
pub fn system_timezone_name() -> String {
    jiff::tz::TimeZone::try_system()
        .ok()
        .and_then(|timezone| timezone.iana_name().map(str::to_owned))
        .unwrap_or_else(|| "UTC".to_string())
}

/// Format a UTC instant as the minute-resolution local value accepted by the
/// ORCH automation form.
pub fn format_local_instant(at_utc: u64, timezone: &str) -> Result<String, Reject> {
    let second = i64::try_from(at_utc)
        .map_err(|_| Reject::new("invalid_time", "time is outside the supported range"))?;
    let local = jiff::Timestamp::from_second(second)
        .map_err(|_| Reject::new("invalid_time", "time is outside the supported range"))?
        .in_tz(timezone)
        .map_err(|_| {
            Reject::new(
                "invalid_timezone",
                format!("unknown IANA timezone: {timezone}"),
            )
        })?;
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute()
    ))
}

/// Parse a local wall-clock instant in `YYYY-MM-DD HH:MM` form using an IANA
/// timezone and return its durable UTC occurrence key.
pub fn parse_local_instant(value: &str, timezone: &str) -> Result<u64, Reject> {
    let (date, time) = value.trim().split_once(' ').ok_or_else(|| {
        Reject::new(
            "invalid_time",
            "time must use YYYY-MM-DD HH:MM in the displayed timezone",
        )
    })?;
    if date.contains(char::is_whitespace)
        || time.contains(char::is_whitespace)
        || time.matches(':').count() != 1
    {
        return Err(Reject::new(
            "invalid_time",
            "time must use YYYY-MM-DD HH:MM in the displayed timezone",
        ));
    }
    parse_wall_time(time)?;
    let local = format!("{date}T{time}:00")
        .parse::<jiff::civil::DateTime>()
        .map_err(|_| {
            Reject::new(
                "invalid_time",
                "time must use YYYY-MM-DD HH:MM in the displayed timezone",
            )
        })?;
    let timestamp = local.in_tz(timezone).map_err(|_| {
        Reject::new(
            "invalid_timezone",
            format!("unknown IANA timezone: {timezone}"),
        )
    })?;
    u64::try_from(timestamp.timestamp().as_second())
        .map_err(|_| Reject::new("invalid_time", "time must be after the Unix epoch"))
}

/// Find the first UTC instant at or after `not_before` whose local minute is
/// `minute`. Repeating from that anchor every 3,600 seconds implements an
/// elapsed-time hourly schedule while giving the form a familiar local clock.
pub fn hourly_anchor(minute: u8, timezone: &str, not_before: u64) -> Result<u64, Reject> {
    if minute > 59 {
        return Err(Reject::new(
            "invalid_time",
            "hourly minute must be between 00 and 59",
        ));
    }
    let second = i64::try_from(not_before)
        .map_err(|_| Reject::new("invalid_time", "time is outside the supported range"))?;
    let local = jiff::Timestamp::from_second(second)
        .map_err(|_| Reject::new("invalid_time", "time is outside the supported range"))?
        .in_tz(timezone)
        .map_err(|_| {
            Reject::new(
                "invalid_timezone",
                format!("unknown IANA timezone: {timezone}"),
            )
        })?;
    let elapsed = u64::from(local.minute() as u8) * 60 + u64::from(local.second() as u8);
    let target = u64::from(minute) * 60;
    let delta = if target >= elapsed {
        target - elapsed
    } else {
        3_600 - (elapsed - target)
    };
    not_before
        .checked_add(delta)
        .ok_or_else(|| Reject::new("invalid_time", "time is outside the supported range"))
}

/// Event-safe definition projection. Prompts, paths, gates, and other task
/// content remain available through explicit reads but never enter the shared
/// event stream or notification payloads.
pub fn definition_event(automation: &Automation) -> serde_json::Value {
    serde_json::json!({
        "id": automation.id,
        "name": automation.name,
        "enabled": automation.enabled,
        "trigger": automation.trigger,
        "target": automation.target,
        "task": {
            "agent_id": automation.task.agent_id,
            "workspace_id": automation.task.workspace_id,
            "mode": automation.task.mode,
            "access": automation.task.access,
        },
        "next_run_at": automation.next_run_at,
    })
}

pub fn parse_utc_instant(value: &str) -> Result<u64, Reject> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Ok(seconds);
    }
    if !value.ends_with('Z') {
        return Err(Reject::new(
            "invalid_time",
            "UTC time must be Unix seconds or RFC 3339 ending in Z",
        ));
    }
    let timestamp = value.parse::<jiff::Timestamp>().map_err(|_| {
        Reject::new(
            "invalid_time",
            "UTC time must be Unix seconds or RFC 3339 ending in Z",
        )
    })?;
    u64::try_from(timestamp.as_second())
        .map_err(|_| Reject::new("invalid_time", "UTC time must be after the Unix epoch"))
}

pub fn parse_wall_time(value: &str) -> Result<u32, Reject> {
    let parts = value.split(':').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len()) {
        return Err(Reject::new(
            "invalid_time",
            "time must use HH:MM or HH:MM:SS",
        ));
    }
    let parse = |index: usize| -> Result<u32, Reject> {
        parts[index]
            .parse::<u32>()
            .map_err(|_| Reject::new("invalid_time", "time must use HH:MM or HH:MM:SS"))
    };
    let hour = parse(0)?;
    let minute = parse(1)?;
    let second = if parts.len() == 3 { parse(2)? } else { 0 };
    if hour > 23 || minute > 59 || second > 59 {
        return Err(Reject::new(
            "invalid_time",
            "time is outside the valid clock range",
        ));
    }
    Ok(hour * 3600 + minute * 60 + second)
}

pub fn parse_weekday(value: &str) -> Result<u8, Reject> {
    match value.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" | "1" => Ok(1),
        "tue" | "tuesday" | "2" => Ok(2),
        "wed" | "wednesday" | "3" => Ok(3),
        "thu" | "thursday" | "4" => Ok(4),
        "fri" | "friday" | "5" => Ok(5),
        "sat" | "saturday" | "6" => Ok(6),
        "sun" | "sunday" | "7" => Ok(7),
        day => Err(Reject::new(
            "invalid_weekday",
            format!("unknown weekday `{day}`"),
        )),
    }
}

fn validate_create(input: &CreateAutomation, now: u64) -> Result<(), Reject> {
    validate_definition(
        &input.name,
        &input.trigger,
        &input.target,
        &input.task,
        &input.policy,
    )?;
    if input.enabled && schedule::first_at_or_after(&input.trigger, now).is_none() {
        return Err(Reject::new(
            "schedule_expired",
            "the schedule has no occurrence at or after the current UTC time",
        ));
    }
    Ok(())
}

fn validate_definition(
    name: &str,
    trigger: &Trigger,
    target: &AutomationTarget,
    task: &TaskTemplate,
    policy: &AutomationPolicy,
) -> Result<(), Reject> {
    validate_text("name", name, MAX_NAME_BYTES)?;
    validate_target(target)?;
    validate_task(task)?;
    if matches!(target, AutomationTarget::ActiveAgent { .. })
        && (!task.paths.is_empty() || task.gate.is_some())
    {
        return Err(Reject::new(
            "invalid_target",
            "active-agent automation does not create ORCH leases or quality gates",
        ));
    }
    validate_policy(policy)?;
    schedule::validate(trigger)
}

fn validate_target(target: &AutomationTarget) -> Result<(), Reject> {
    let AutomationTarget::ActiveAgent {
        pane_id,
        terminal_id,
        ..
    } = target
    else {
        return Ok(());
    };
    if *pane_id == 0 {
        return Err(Reject::new(
            "invalid_target",
            "active-agent pane_id must be non-zero",
        ));
    }
    if terminal_id.len() != 32
        || !terminal_id
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Reject::new(
            "invalid_target",
            "active-agent terminal_id must be 32 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_task(task: &TaskTemplate) -> Result<(), Reject> {
    validate_text("task.title", &task.title, MAX_TITLE_BYTES)?;
    validate_text("task.prompt", &task.prompt, MAX_PROMPT_BYTES)?;
    if contains_unsafe_prompt_control(&task.prompt) {
        return Err(Reject::new(
            "invalid_prompt",
            "task.prompt must not contain terminal control characters other than newlines",
        ));
    }
    validate_text("task.agent_id", &task.agent_id, 64)?;
    validate_text("task.workspace_id", &task.workspace_id, 128)?;
    if task.paths.len() > crate::orch::MAX_LEASE_PATHS {
        return Err(Reject::new(
            "path_limit",
            format!(
                "task.paths must contain at most {} entries",
                crate::orch::MAX_LEASE_PATHS
            ),
        ));
    }
    for path in &task.paths {
        if path.trim().is_empty() {
            return Err(Reject::new(
                "bad_request",
                "task.paths cannot contain blanks",
            ));
        }
        if path.len() > crate::orch::MAX_LEASE_PATH_BYTES {
            return Err(Reject::new(
                "path_limit",
                format!(
                    "each task path must be at most {} bytes",
                    crate::orch::MAX_LEASE_PATH_BYTES
                ),
            ));
        }
    }
    if task
        .gate
        .as_ref()
        .is_some_and(|gate| gate.len() > MAX_GATE_BYTES)
    {
        return Err(Reject::new(
            "field_limit",
            format!("task.gate must be at most {MAX_GATE_BYTES} bytes"),
        ));
    }
    Ok(())
}

pub(crate) fn contains_unsafe_prompt_control(value: &str) -> bool {
    value
        .chars()
        .any(|value| value.is_control() && value != '\n')
}

fn validate_policy(policy: &AutomationPolicy) -> Result<(), Reject> {
    if policy.misfire_grace_seconds > 31_536_000 {
        return Err(Reject::new(
            "invalid_policy",
            "misfire_grace_seconds must not exceed one year",
        ));
    }
    Ok(())
}

fn validate_identifier(field: &str, value: &str) -> Result<(), Reject> {
    validate_text(field, value, 128)?;
    if value.chars().any(char::is_control) {
        return Err(Reject::new(
            "invalid_ledger",
            format!("{field} must not contain control characters"),
        ));
    }
    Ok(())
}

fn create_fingerprint(input: &CreateAutomation) -> String {
    serde_json::to_string(&(
        &input.name,
        input.enabled,
        &input.trigger,
        &input.target,
        &input.task,
        &input.policy,
    ))
    .unwrap_or_default()
}

fn validate_text(field: &str, value: &str, max: usize) -> Result<(), Reject> {
    if value.trim().is_empty() {
        return Err(Reject::new("bad_request", format!("{field} is required")));
    }
    if value.len() > max {
        return Err(Reject::new(
            "field_limit",
            format!("{field} must be at most {max} bytes"),
        ));
    }
    Ok(())
}

fn truncate(mut value: String, max: usize) -> String {
    if value.len() <= max {
        return value;
    }
    const ELLIPSIS: &str = "…";
    let with_ellipsis = max >= ELLIPSIS.len();
    let mut cut = max.saturating_sub(if with_ellipsis { ELLIPSIS.len() } else { 0 });
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    value.truncate(cut);
    if with_ellipsis {
        value.push_str(ELLIPSIS);
    }
    value
}

fn max_numeric_id<T>(items: &[T], id: impl Fn(&T) -> &str) -> u64 {
    items
        .iter()
        .filter_map(|item| id(item).get(1..)?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(trigger: Trigger) -> CreateAutomation {
        CreateAutomation {
            name: "Morning review".into(),
            enabled: true,
            trigger,
            target: AutomationTarget::NewWorker,
            task: TaskTemplate {
                title: "Review open work".into(),
                prompt: "Review the current changes and report risks.".into(),
                agent_id: "codex".into(),
                workspace_id: "workspace-1".into(),
                mode: crate::orch::TaskWorkerMode::Workspace,
                access: AutomationAccess::Workspace,
                paths: vec![],
                gate: None,
            },
            policy: AutomationPolicy::default(),
        }
    }

    #[test]
    fn create_is_idempotent_and_collision_safe() {
        let mut state = AutomationState::default();
        let trigger = Trigger::Once { at_utc: 100 };
        let first = state
            .create(input(trigger.clone()), Some("request-1"), 10)
            .unwrap();
        let again = state
            .create(input(trigger), Some("request-1"), 200)
            .unwrap();
        assert_eq!(first.id, again.id);
        let mut changed = input(Trigger::Once { at_utc: 100 });
        changed.name = "Different".into();
        assert_eq!(
            state
                .create(changed, Some("request-1"), 10)
                .unwrap_err()
                .code,
            "idempotency_conflict"
        );
    }

    #[test]
    fn legacy_task_templates_default_to_workspace_access() {
        let task: TaskTemplate = serde_json::from_value(serde_json::json!({
            "title":"Review",
            "prompt":"Review changes",
            "agent_id":"codex",
            "workspace_id":"workspace-1",
            "mode":"workspace"
        }))
        .unwrap();
        assert_eq!(task.access, AutomationAccess::Workspace);
    }

    #[test]
    fn legacy_definitions_and_runs_default_to_new_workers() {
        let mut state = AutomationState::default();
        let automation = state
            .create(input(Trigger::Once { at_utc: 100 }), None, 10)
            .unwrap();
        let run = state.request_run(&automation.id, None, 20).unwrap();
        let mut definition = serde_json::to_value(&automation).unwrap();
        let mut run_value = serde_json::to_value(&run).unwrap();
        definition.as_object_mut().unwrap().remove("target");
        run_value.as_object_mut().unwrap().remove("target");

        let definition: Automation = serde_json::from_value(definition).unwrap();
        let run: AutomationRun = serde_json::from_value(run_value).unwrap();
        assert_eq!(definition.target, AutomationTarget::NewWorker);
        assert_eq!(run.target, AutomationTarget::NewWorker);
    }

    #[test]
    fn active_agent_target_requires_a_lowercase_terminal_lifetime_id() {
        let mut state = AutomationState::default();
        let mut definition = input(Trigger::Once { at_utc: 100 });
        definition.target = AutomationTarget::ActiveAgent {
            pane_id: 7,
            terminal_id: "not-a-terminal".into(),
            if_busy: ActiveAgentBusyPolicy::Wait,
        };
        let error = state.create(definition, None, 10).unwrap_err();

        assert_eq!(error.code, "invalid_target");

        let mut valid = input(Trigger::Once { at_utc: 100 });
        valid.target = AutomationTarget::ActiveAgent {
            pane_id: 7,
            terminal_id: "0123456789abcdef0123456789abcdef".into(),
            if_busy: ActiveAgentBusyPolicy::Skip,
        };
        assert!(state.create(valid, None, 10).is_ok());
    }

    #[test]
    fn pre_rename_active_agent_target_alias_loads_but_serializes_canonically() {
        let target: AutomationTarget = serde_json::from_value(serde_json::json!({
            "kind":"existing_agent",
            "pane_id":7,
            "terminal_id":"0123456789abcdef0123456789abcdef",
            "if_busy":"wait"
        }))
        .unwrap();

        assert!(matches!(target, AutomationTarget::ActiveAgent { .. }));
        assert_eq!(
            serde_json::to_value(target).unwrap()["kind"],
            "active_agent"
        );
    }

    #[test]
    fn create_accepts_newlines_but_rejects_other_terminal_controls_in_prompt() {
        let mut state = AutomationState::default();
        let mut definition = input(Trigger::Once { at_utc: 100 });
        definition.task.prompt = "review this\nthen run something".into();

        let automation = state.create(definition.clone(), None, 10).unwrap();
        assert_eq!(automation.task.prompt, "review this\nthen run something");

        definition.name = "unsafe review".into();
        definition.task.prompt = "review this\x1bthen run something".into();
        let error = state.create(definition, None, 10).unwrap_err();

        assert_eq!(error.code, "invalid_prompt");
        assert_eq!(state.automations.len(), 1);
    }

    #[test]
    fn run_retry_returns_the_original_occurrence() {
        let mut state = AutomationState::default();
        let automation = state
            .create(input(Trigger::Once { at_utc: 100 }), None, 10)
            .unwrap();
        let run = state
            .request_run(&automation.id, Some("run-request-1"), 20)
            .unwrap();

        assert_eq!(
            state
                .run_retry(&automation.id, Some("run-request-1"))
                .unwrap()
                .unwrap()
                .id,
            run.id
        );
        assert_eq!(state.runs.len(), 1);
        assert_eq!(
            state
                .run_retry("a-different", Some("run-request-1"))
                .unwrap_err()
                .code,
            "idempotency_conflict"
        );
    }

    #[test]
    fn run_snapshots_trigger_policy_and_task_before_definition_edits() {
        let mut state = AutomationState::default();
        let original_trigger = Trigger::Interval {
            every_seconds: 60,
            anchor_utc: 100,
        };
        let mut original = input(original_trigger.clone());
        original.policy.overlap = OverlapPolicy::QueueOne;
        let automation = state.create(original.clone(), None, 10).unwrap();
        let run = state.request_run(&automation.id, None, 20).unwrap();

        let mut changed = input(Trigger::Once { at_utc: 500 });
        changed.task.prompt = "A later briefing".into();
        state.update(&automation.id, changed, 30).unwrap();

        let persisted = state.run(&run.id).unwrap();
        assert_eq!(persisted.trigger.as_ref(), Some(&original_trigger));
        assert_eq!(persisted.policy, original.policy);
        assert_eq!(persisted.target, original.target);
        assert_eq!(persisted.task.prompt, original.task.prompt);
    }

    #[test]
    fn due_occurrence_is_created_once_and_schedule_advances() {
        let mut state = AutomationState::default();
        let automation = state
            .create(
                input(Trigger::Interval {
                    every_seconds: 60,
                    anchor_utc: 100,
                }),
                None,
                10,
            )
            .unwrap();
        let first = state.collect_due(100);
        assert_eq!(first.len(), 1);
        assert!(state.collect_due(100).is_empty());
        assert_eq!(
            state.automation(&automation.id).unwrap().next_run_at,
            Some(160)
        );
    }

    #[test]
    fn run_latest_materializes_only_the_newest_missed_occurrence() {
        let mut state = AutomationState::default();
        state
            .create(
                input(Trigger::Interval {
                    every_seconds: 60,
                    anchor_utc: 100,
                }),
                None,
                10,
            )
            .unwrap();

        let created = state.collect_due(299);
        assert_eq!(created.len(), 1);
        assert_eq!(state.run(&created[0]).unwrap().scheduled_at, 280);
        assert_eq!(state.automation("a1").unwrap().next_run_at, Some(340));
    }

    #[test]
    fn overlap_is_recorded_without_a_second_pending_run() {
        let mut state = AutomationState::default();
        state
            .create(
                input(Trigger::Interval {
                    every_seconds: 60,
                    anchor_utc: 100,
                }),
                None,
                10,
            )
            .unwrap();
        state.collect_due(100);
        assert!(state.collect_due(160).is_empty());
        assert_eq!(state.runs.last().unwrap().status, RunStatus::Skipped);
    }

    #[test]
    fn queue_one_never_accumulates_more_than_one_pending_occurrence() {
        let mut state = AutomationState::default();
        let mut definition = input(Trigger::Interval {
            every_seconds: 60,
            anchor_utc: 100,
        });
        definition.policy.overlap = OverlapPolicy::QueueOne;
        state.create(definition, None, 10).unwrap();
        let first = state.collect_due(100)[0].clone();
        state
            .set_run_status(&first, RunStatus::Running, None, 100)
            .unwrap();
        assert_eq!(state.collect_due(160).len(), 1);
        assert!(state.collect_due(220).is_empty());
        assert_eq!(
            state
                .runs
                .iter()
                .filter(|run| run.status == RunStatus::Pending)
                .count(),
            1
        );
        assert_eq!(state.runs.last().unwrap().status, RunStatus::Skipped);
    }

    #[test]
    fn one_shot_disables_after_its_occurrence() {
        let mut state = AutomationState::default();
        let automation = state
            .create(input(Trigger::Once { at_utc: 100 }), None, 10)
            .unwrap();
        state.collect_due(100);
        let automation = state.automation(&automation.id).unwrap();
        assert!(!automation.enabled);
        assert_eq!(automation.next_run_at, None);
    }

    #[test]
    fn disabling_cancels_queued_work_and_expired_once_cannot_be_reenabled() {
        let mut state = AutomationState::default();
        let automation = state
            .create(input(Trigger::Once { at_utc: 100 }), None, 10)
            .unwrap();
        let run = state.request_run(&automation.id, None, 20).unwrap();

        state.set_enabled(&automation.id, false, 30).unwrap();
        assert_eq!(state.run(&run.id).unwrap().status, RunStatus::Cancelled);
        assert!(state.pending_runs().is_empty());
        assert_eq!(
            state
                .set_enabled(&automation.id, true, 101)
                .unwrap_err()
                .code,
            "schedule_expired"
        );
    }

    #[test]
    fn absolute_times_are_explicitly_utc() {
        assert_eq!(
            parse_utc_instant("2026-09-03T12:00:00Z").unwrap(),
            1_788_436_800
        );
        assert_eq!(
            parse_utc_instant("2026-09-03T20:00:00+08:00")
                .unwrap_err()
                .code,
            "invalid_time"
        );
        let utc = parse_utc_instant("2026-09-03T00:00:00Z").unwrap();
        assert_eq!(
            format_local_instant(utc, "Asia/Makassar").unwrap(),
            "2026-09-03 08:00"
        );
        assert_eq!(
            parse_local_instant("2026-09-03 08:00", "Asia/Makassar").unwrap(),
            utc
        );
    }

    #[test]
    fn shared_definition_events_exclude_task_content() {
        let mut input = input(Trigger::Once { at_utc: 100 });
        input.task.paths = vec!["private/path".into()];
        input.task.gate = Some("secret gate instructions".into());
        let mut state = AutomationState::default();
        let automation = state.create(input, None, 10).unwrap();
        let event = definition_event(&automation).to_string();

        assert!(!event.contains("Review the current changes"));
        assert!(!event.contains("private/path"));
        assert!(!event.contains("secret gate instructions"));
        assert_eq!(event.matches("workspace-1").count(), 1);
        assert!(event.contains("\"access\":\"workspace\""));
    }

    #[test]
    fn persisted_run_error_truncation_includes_the_ellipsis_in_its_limit() {
        let mut state = AutomationState::default();
        let automation = state
            .create(input(Trigger::Once { at_utc: 100 }), None, 10)
            .unwrap();
        let run = state.request_run(&automation.id, None, 20).unwrap();
        let oversized = "é".repeat(MAX_ERROR_BYTES);

        let updated = state
            .set_run_status(&run.id, RunStatus::Failed, Some(oversized), 30)
            .unwrap();
        let error = updated.error.expect("failed run keeps a bounded error");

        assert!(error.len() <= MAX_ERROR_BYTES);
        assert!(error.ends_with('…'));
        assert!(state.validate_loaded().is_ok());
        assert_eq!(truncate("é".into(), 1), "");
    }

    #[test]
    fn persistence_restores_ids_and_deadline_cache() {
        let _env = crate::persist::test_env("automation-persistence");
        let path = crate::persist::session_dir().join("automations-test.json");
        let mut state = AutomationState::load_from(path.clone());
        let automation = state
            .create(input(Trigger::Once { at_utc: 100 }), None, 10)
            .unwrap();
        state.save().unwrap();
        let mut restored = AutomationState::load_from(path);
        assert_eq!(restored.next_deadline(), Some(100));
        assert_eq!(
            restored.automation(&automation.id).unwrap().name,
            "Morning review"
        );
        let second = restored
            .create(input(Trigger::Once { at_utc: 200 }), None, 10)
            .unwrap();
        assert_eq!(second.id, "a2");
    }
}
