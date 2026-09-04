//! Calendar-safe schedule arithmetic.
//!
//! Every durable occurrence is a UTC Unix timestamp. Daily and weekly triggers
//! retain an IANA timezone so their next local wall-clock occurrence remains
//! stable through daylight-saving transitions.

use super::model::{Reject, Trigger, MIN_INTERVAL_SECONDS};
use jiff::{civil::Date, Timestamp};

const DAY_SECONDS: u64 = 24 * 60 * 60;

pub fn validate(trigger: &Trigger) -> Result<(), Reject> {
    match trigger {
        Trigger::Once { at_utc } if *at_utc == 0 => {
            Err(Reject::new("invalid_schedule", "at_utc must be positive"))
        }
        Trigger::Interval { anchor_utc, .. } if *anchor_utc == 0 => Err(Reject::new(
            "invalid_schedule",
            "anchor_utc must be positive",
        )),
        Trigger::Interval { every_seconds, .. } if *every_seconds < MIN_INTERVAL_SECONDS => {
            Err(Reject::new(
                "invalid_schedule",
                format!("every_seconds must be at least {MIN_INTERVAL_SECONDS}"),
            ))
        }
        Trigger::Daily {
            timezone,
            second_of_day,
        } => validate_local(timezone, *second_of_day),
        Trigger::Weekly {
            timezone,
            weekdays,
            second_of_day,
        } => {
            validate_local(timezone, *second_of_day)?;
            if weekdays.is_empty() || weekdays.iter().any(|day| !(1..=7).contains(day)) {
                return Err(Reject::new(
                    "invalid_schedule",
                    "weekdays must contain ISO days 1 through 7",
                ));
            }
            let mut unique = weekdays.clone();
            unique.sort_unstable();
            unique.dedup();
            if unique.len() != weekdays.len() {
                return Err(Reject::new(
                    "invalid_schedule",
                    "weekdays must not contain duplicates",
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Return the first occurrence at or after `not_before`.
pub fn first_at_or_after(trigger: &Trigger, not_before: u64) -> Option<u64> {
    match trigger {
        Trigger::Once { at_utc } => (*at_utc >= not_before).then_some(*at_utc),
        Trigger::Interval {
            every_seconds,
            anchor_utc,
        } => {
            if *every_seconds == 0 {
                return None;
            }
            if *anchor_utc >= not_before {
                return Some(*anchor_utc);
            }
            let elapsed = not_before - *anchor_utc;
            let steps = elapsed.div_ceil(*every_seconds);
            anchor_utc.checked_add(steps.checked_mul(*every_seconds)?)
        }
        Trigger::Daily {
            timezone,
            second_of_day,
        } => next_local(timezone, *second_of_day, None, not_before),
        Trigger::Weekly {
            timezone,
            weekdays,
            second_of_day,
        } => next_local(timezone, *second_of_day, Some(weekdays), not_before),
    }
}

pub fn next_after(trigger: &Trigger, after: u64) -> Option<u64> {
    first_at_or_after(trigger, after.checked_add(1)?)
}

/// Return the newest occurrence no later than `at`. Calendar schedules need
/// inspect at most eight days because their longest gap is one week; interval
/// schedules use direct arithmetic regardless of downtime length.
pub fn latest_at_or_before(trigger: &Trigger, at: u64) -> Option<u64> {
    match trigger {
        Trigger::Once { at_utc } => (*at_utc <= at).then_some(*at_utc),
        Trigger::Interval {
            every_seconds,
            anchor_utc,
        } => {
            if *every_seconds == 0 {
                return None;
            }
            if *anchor_utc > at {
                return None;
            }
            let steps = (at - *anchor_utc) / *every_seconds;
            anchor_utc.checked_add(steps.checked_mul(*every_seconds)?)
        }
        Trigger::Daily { .. } | Trigger::Weekly { .. } => {
            let mut latest = None;
            let mut candidate = first_at_or_after(trigger, at.saturating_sub(8 * DAY_SECONDS));
            while let Some(occurrence) = candidate {
                if occurrence > at {
                    break;
                }
                latest = Some(occurrence);
                candidate = next_after(trigger, occurrence);
            }
            latest
        }
    }
}

pub fn preview(trigger: &Trigger, not_before: u64, limit: usize) -> Vec<u64> {
    let mut out = Vec::with_capacity(limit.min(32));
    let mut next = first_at_or_after(trigger, not_before);
    while let Some(at) = next {
        if out.len() >= limit.min(32) {
            break;
        }
        out.push(at);
        next = next_after(trigger, at);
    }
    out
}

fn validate_local(timezone: &str, second_of_day: u32) -> Result<(), Reject> {
    if u64::from(second_of_day) >= DAY_SECONDS {
        return Err(Reject::new(
            "invalid_schedule",
            "second_of_day must be below 86400",
        ));
    }
    if timezone.trim().is_empty() || timezone.len() > 128 || jiff::tz::db().get(timezone).is_err() {
        return Err(Reject::new(
            "invalid_timezone",
            format!("unknown IANA timezone: {timezone}"),
        ));
    }
    Ok(())
}

fn next_local(
    timezone: &str,
    second_of_day: u32,
    weekdays: Option<&[u8]>,
    not_before: u64,
) -> Option<u64> {
    let now = Timestamp::new(i64::try_from(not_before).ok()?, 0)
        .ok()?
        .in_tz(timezone)
        .ok()?;
    let mut date = now.date();
    for _ in 0..=7 {
        if weekdays.is_none_or(|allowed| allowed.contains(&(date.weekday() as u8))) {
            let candidate = local_candidate(date, timezone, second_of_day)?;
            if candidate >= not_before {
                return Some(candidate);
            }
        }
        date = date.tomorrow().ok()?;
    }
    None
}

fn local_candidate(date: Date, timezone: &str, second_of_day: u32) -> Option<u64> {
    let hour = (second_of_day / 3600) as i8;
    let minute = ((second_of_day % 3600) / 60) as i8;
    let second = (second_of_day % 60) as i8;
    let timestamp = date
        .at(hour, minute, second, 0)
        .in_tz(timezone)
        .ok()?
        .timestamp()
        .as_second();
    u64::try_from(timestamp).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_rolls_forward_in_utc() {
        let trigger = Trigger::Daily {
            timezone: "UTC".into(),
            second_of_day: 8 * 3600,
        };
        assert_eq!(first_at_or_after(&trigger, 7 * 3600), Some(8 * 3600));
        assert_eq!(first_at_or_after(&trigger, 9 * 3600), Some(32 * 3600));
    }

    #[test]
    fn weekly_uses_iso_weekdays() {
        let trigger = Trigger::Weekly {
            timezone: "UTC".into(),
            weekdays: vec![1, 5],
            second_of_day: 0,
        };
        // Epoch day 0 is Thursday; day 1 is Friday and day 4 is Monday.
        assert_eq!(first_at_or_after(&trigger, 0), Some(DAY_SECONDS));
        assert_eq!(next_after(&trigger, DAY_SECONDS), Some(4 * DAY_SECONDS));
    }

    #[test]
    fn interval_skips_directly_to_nearest_occurrence() {
        let trigger = Trigger::Interval {
            every_seconds: 60,
            anchor_utc: 100,
        };
        assert_eq!(first_at_or_after(&trigger, 100), Some(100));
        assert_eq!(first_at_or_after(&trigger, 221), Some(280));
        assert_eq!(latest_at_or_before(&trigger, 279), Some(220));
    }

    #[test]
    fn daily_preserves_wall_clock_across_dst() {
        let trigger = Trigger::Daily {
            timezone: "America/New_York".into(),
            second_of_day: 9 * 3600,
        };
        // 09:00 is 14:00 UTC before the 2024 spring change and 13:00 after it.
        assert_eq!(
            first_at_or_after(&trigger, 1_709_990_000),
            Some(1_709_992_800)
        );
        assert_eq!(next_after(&trigger, 1_709_992_800), Some(1_710_075_600));
    }

    #[test]
    fn dst_gap_moves_forward_and_fold_runs_only_the_earlier_instant() {
        let seconds = 2 * 3600 + 30 * 60;
        let trigger = Trigger::Daily {
            timezone: "America/New_York".into(),
            second_of_day: seconds,
        };
        let spring_start = "2024-03-10T00:00:00Z"
            .parse::<Timestamp>()
            .unwrap()
            .as_second() as u64;
        let spring = first_at_or_after(&trigger, spring_start).unwrap();
        assert_eq!(
            Timestamp::from_second(spring as i64).unwrap().to_string(),
            "2024-03-10T07:30:00Z"
        );

        let fold_trigger = Trigger::Daily {
            timezone: "America/New_York".into(),
            second_of_day: 3600 + 30 * 60,
        };
        let fall_start = "2024-11-03T00:00:00Z"
            .parse::<Timestamp>()
            .unwrap()
            .as_second() as u64;
        let fall = first_at_or_after(&fold_trigger, fall_start).unwrap();
        assert_eq!(
            Timestamp::from_second(fall as i64).unwrap().to_string(),
            "2024-11-03T05:30:00Z"
        );
        assert!(next_after(&fold_trigger, fall).unwrap() > fall + 23 * 3600);
    }

    #[test]
    fn zero_interval_never_schedules() {
        let trigger = Trigger::Interval {
            every_seconds: 0,
            anchor_utc: 100,
        };
        assert_eq!(first_at_or_after(&trigger, 0), None);
        assert_eq!(latest_at_or_before(&trigger, 200), None);
    }

    #[test]
    fn validation_rejects_fast_or_invalid_schedules() {
        assert_eq!(
            validate(&Trigger::Interval {
                every_seconds: 1,
                anchor_utc: 10,
            })
            .unwrap_err()
            .code,
            "invalid_schedule"
        );
        assert!(validate(&Trigger::Weekly {
            timezone: "UTC".into(),
            weekdays: vec![0],
            second_of_day: 0,
        })
        .is_err());
    }

    #[test]
    fn rejects_unknown_timezone() {
        assert_eq!(
            validate(&Trigger::Daily {
                timezone: "Mars/Olympus".into(),
                second_of_day: 0,
            })
            .unwrap_err()
            .code,
            "invalid_timezone"
        );
    }
}
