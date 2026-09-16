use crate::{Calendar, CalendarError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum ScheduleTiming {
    Cron {
        cron_expr: String,
        tz: String,
    },
    Interval {
        interval_ms: i64,
        anchor: IntervalAnchor,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntervalAnchor {
    Epoch,
    Deploy,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleOverlap {
    #[default]
    Allow,
    SkipIfRunning,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum ScheduleCatchUp {
    #[default]
    Skip,
    Backfill {
        max: usize,
    },
}

impl<'de> Deserialize<'de> for ScheduleCatchUp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
        enum Wire {
            Skip {},
            Backfill { max: usize },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Skip {} => Self::Skip,
            Wire::Backfill { max } => Self::Backfill { max },
        })
    }
}

impl ScheduleTiming {
    /// Return the first occurrence strictly after `after`, in Unix milliseconds.
    /// Deployment-anchored intervals use the supplied activation instant; epoch
    /// intervals and cron schedules do not depend on it.
    ///
    /// # Errors
    /// Returns an error for invalid schedule definitions, timestamp arithmetic
    /// overflow or an unrepresentable next occurrence.
    pub fn next_after(&self, after: i64, activated_at: i64) -> Result<i64, CalendarError> {
        let overflow =
            || CalendarError::InvalidSchedule("workflow schedule timestamp is out of range".into());
        match self {
            Self::Cron { cron_expr, tz } => Calendar::parse(cron_expr, tz)?
                .next_after(DateTime::<Utc>::from_timestamp_millis(after).ok_or_else(overflow)?)
                .map(|time| time.timestamp_millis()),
            Self::Interval {
                interval_ms,
                anchor,
            } => {
                if *interval_ms <= 0 {
                    return Err(CalendarError::InvalidSchedule(
                        "workflow interval must be positive".into(),
                    ));
                }
                let anchor = if *anchor == IntervalAnchor::Epoch {
                    0
                } else {
                    activated_at
                };
                let elapsed = after.checked_sub(anchor).ok_or_else(overflow)?;
                let periods = elapsed
                    .div_euclid(*interval_ms)
                    .checked_add(1)
                    .ok_or_else(overflow)?;
                let next = anchor
                    .checked_add(periods.checked_mul(*interval_ms).ok_or_else(overflow)?)
                    .ok_or_else(overflow)?;
                DateTime::<Utc>::from_timestamp_millis(next).ok_or_else(overflow)?;
                Ok(next)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interval_frontier_is_strict_and_uses_the_selected_anchor() {
        let epoch = ScheduleTiming::Interval {
            interval_ms: 1_000,
            anchor: IntervalAnchor::Epoch,
        };
        let deploy = ScheduleTiming::Interval {
            interval_ms: 1_000,
            anchor: IntervalAnchor::Deploy,
        };
        assert_eq!(epoch.next_after(2_000, 2_500).unwrap(), 3_000);
        assert_eq!(epoch.next_after(2_001, 2_500).unwrap(), 3_000);
        assert_eq!(deploy.next_after(2_500, 2_500).unwrap(), 3_500);
        assert_eq!(deploy.next_after(3_499, 2_500).unwrap(), 3_500);
        assert_eq!(deploy.next_after(3_500, 2_500).unwrap(), 4_500);
    }

    #[test]
    fn interval_alignment_uses_euclidean_division_before_the_anchor() {
        let epoch = ScheduleTiming::Interval {
            interval_ms: 1_000,
            anchor: IntervalAnchor::Epoch,
        };
        let deploy = ScheduleTiming::Interval {
            interval_ms: 1_000,
            anchor: IntervalAnchor::Deploy,
        };
        assert_eq!(epoch.next_after(-1_001, 0).unwrap(), -1_000);
        assert_eq!(epoch.next_after(-1_000, 0).unwrap(), 0);
        assert_eq!(epoch.next_after(-1, 0).unwrap(), 0);
        assert_eq!(deploy.next_after(-1, 500).unwrap(), 500);
        assert_eq!(deploy.next_after(0, 2_500).unwrap(), 500);
    }

    #[test]
    fn interval_rejects_invalid_periods_and_unrepresentable_results() {
        for interval_ms in [0, -1, i64::MIN] {
            assert!(ScheduleTiming::Interval {
                interval_ms,
                anchor: IntervalAnchor::Epoch,
            }
            .next_after(0, 0)
            .is_err());
        }
        let interval = ScheduleTiming::Interval {
            interval_ms: 1_000,
            anchor: IntervalAnchor::Deploy,
        };
        for (after, activation) in [
            (i64::MAX, i64::MIN),
            (i64::MIN, i64::MAX),
            (i64::MAX, 0),
            (i64::MAX - 1_000, 0),
        ] {
            assert!(interval.next_after(after, activation).is_err());
        }
    }

    #[test]
    fn cron_timing_uses_the_calendar_timezone_and_ignores_the_interval_anchor() {
        let cron = ScheduleTiming::Cron {
            cron_expr: "30 2 * * *".into(),
            tz: "America/New_York".into(),
        };
        let after = DateTime::parse_from_rfc3339("2026-03-08T06:00:00Z")
            .unwrap()
            .timestamp_millis();
        let expected = DateTime::parse_from_rfc3339("2026-03-08T07:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(cron.next_after(after, 0).unwrap(), expected);
        assert_eq!(cron.next_after(after, i64::MAX).unwrap(), expected);
        assert!(cron.next_after(i64::MAX, 0).is_err());
    }

    #[test]
    fn schedule_metadata_has_explicit_serialization_contracts() {
        let interval = ScheduleTiming::Interval {
            interval_ms: 5_000,
            anchor: IntervalAnchor::Deploy,
        };
        let wire = json!({"kind":"interval", "interval_ms":5_000, "anchor":"deploy"});
        assert_eq!(serde_json::to_value(&interval).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ScheduleTiming>(wire).unwrap(),
            interval
        );
        assert_eq!(
            serde_json::to_value(ScheduleTiming::Cron {
                cron_expr: "@daily".into(),
                tz: "UTC".into(),
            })
            .unwrap(),
            json!({"kind":"cron", "cron_expr":"@daily", "tz":"UTC"})
        );
        assert_eq!(
            serde_json::to_value(ScheduleOverlap::SkipIfRunning).unwrap(),
            json!("skipIfRunning")
        );
        assert_eq!(
            serde_json::to_value(ScheduleCatchUp::Backfill { max: 3 }).unwrap(),
            json!({"mode":"backfill", "max":3})
        );
        assert_eq!(
            serde_json::to_value(ScheduleCatchUp::default()).unwrap(),
            json!({"mode":"skip"})
        );
        assert_eq!(ScheduleOverlap::default(), ScheduleOverlap::Allow);
        assert_eq!(
            serde_json::from_value::<ScheduleCatchUp>(json!({"mode":"skip"})).unwrap(),
            ScheduleCatchUp::Skip
        );
        assert!(serde_json::from_value::<ScheduleTiming>(json!({
            "kind":"interval", "interval_ms":5_000, "anchor":"deploy", "unknown":true,
        }))
        .is_err());
        assert!(serde_json::from_value::<ScheduleCatchUp>(json!({
            "mode":"backfill", "max":3, "unknown":true,
        }))
        .is_err());
    }

    #[test]
    fn skip_catch_up_rejects_unknown_fields() {
        assert!(serde_json::from_value::<ScheduleCatchUp>(json!({
            "mode":"skip", "max":3,
        }))
        .is_err());
        assert!(serde_json::from_value::<ScheduleCatchUp>(json!({
            "mode":"skip", "unknown":null,
        }))
        .is_err());
    }
}
