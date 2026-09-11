//! Calendar scheduling shared by embedded and deployed workflow hosts.
use crate::WorkflowServiceError;
use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, LocalResult, NaiveDate, NaiveDateTime,
    TimeZone, Utc,
};
use chrono_tz::Tz;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct Calendar {
    expression: String,
    timezone: Tz,
    cron: Box<ParsedCron>,
}
impl Calendar {
    pub fn parse(expression: &str, timezone: &str) -> Result<Self, WorkflowServiceError> {
        if expression.len() > 256 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow cron expression is too long".into(),
            ));
        }
        let expression = normalize_cron_expr(expression)?;
        let timezone = timezone.parse::<Tz>().map_err(|_| {
            WorkflowServiceError::InvalidRequest("unknown workflow schedule timezone".into())
        })?;
        let cron = Box::new(parse_cron_expr(&expression)?);
        Ok(Self {
            expression,
            timezone,
            cron,
        })
    }
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }
    #[must_use]
    pub fn timezone(&self) -> &str {
        self.timezone.name()
    }
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>, WorkflowServiceError> {
        first_cron_fire_after(&self.cron, self.timezone, after)
    }
}
#[derive(Debug, Clone)]
struct ParsedCron {
    minutes: CronField,
    hours: CronField,
    days_of_month: CronField,
    months: CronField,
    days_of_week: CronField,
}

#[derive(Debug, Clone)]
struct CronField {
    values: BTreeSet<u32>,
    wildcard: bool,
}

fn first_cron_fire_after(
    cron: &ParsedCron,
    tz: Tz,
    after: DateTime<Utc>,
) -> Result<DateTime<Utc>, WorkflowServiceError> {
    let local_after = after.with_timezone(&tz);
    let start = local_after.date_naive();
    for day_offset in 0..=(366 * 8) {
        let Some(date) = start.checked_add_signed(ChronoDuration::days(day_offset)) else {
            break;
        };
        if !cron.date_matches(date) {
            continue;
        }
        for hour in &cron.hours.values {
            for minute in &cron.minutes.values {
                let Some(nominal) = date.and_hms_opt(*hour, *minute, 0) else {
                    continue;
                };
                let Some(candidate) = resolve_local_nominal(tz, nominal) else {
                    continue;
                };
                if candidate > after {
                    return Ok(candidate);
                }
            }
        }
    }
    Err(WorkflowServiceError::InvalidRequest(
        "workflow schedule has no next cron fire within the search horizon".to_string(),
    ))
}

fn resolve_local_nominal(tz: Tz, nominal: NaiveDateTime) -> Option<DateTime<Utc>> {
    match tz.from_local_datetime(&nominal) {
        LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(a, b) => {
            let a = a.with_timezone(&Utc);
            let b = b.with_timezone(&Utc);
            Some(a.min(b))
        }
        LocalResult::None => {
            let mut probe = nominal;
            for _ in 0..(48 * 60) {
                probe = probe.checked_add_signed(ChronoDuration::minutes(1))?;
                match tz.from_local_datetime(&probe) {
                    LocalResult::Single(dt) => return Some(dt.with_timezone(&Utc)),
                    LocalResult::Ambiguous(a, b) => {
                        let a = a.with_timezone(&Utc);
                        let b = b.with_timezone(&Utc);
                        return Some(a.min(b));
                    }
                    LocalResult::None => {}
                }
            }
            None
        }
    }
}

fn normalize_cron_expr(expr: &str) -> Result<String, WorkflowServiceError> {
    let compact = expr.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflow schedule cron expression is empty".to_string(),
        ));
    }
    let normalized = match compact.as_str() {
        "@hourly" => "0 * * * *".to_string(),
        "@daily" => "0 0 * * *".to_string(),
        "@weekly" => "0 0 * * 0".to_string(),
        "@monthly" => "0 0 1 * *".to_string(),
        "@yearly" => "0 0 1 1 *".to_string(),
        value if value.starts_with('@') => {
            return Err(WorkflowServiceError::InvalidRequest(format!(
                "unsupported workflow schedule cron macro {value:?}"
            )))
        }
        _ => compact,
    };
    let field_count = normalized.split_whitespace().count();
    if field_count == 6 {
        return Err(WorkflowServiceError::InvalidRequest(
            "sub-minute workflow schedule cron expressions are unsupported".to_string(),
        ));
    }
    if field_count != 5 {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflow schedule cron expression must have exactly 5 fields".to_string(),
        ));
    }
    Ok(normalized)
}

fn parse_cron_expr(expr: &str) -> Result<ParsedCron, WorkflowServiceError> {
    let normalized = normalize_cron_expr(expr)?;
    let fields = normalized.split_whitespace().collect::<Vec<_>>();
    Ok(ParsedCron {
        minutes: parse_cron_field(fields[0], "minute", 0, 59, None)?,
        hours: parse_cron_field(fields[1], "hour", 0, 23, None)?,
        days_of_month: parse_cron_field(fields[2], "day-of-month", 1, 31, Some(28))?,
        months: parse_cron_field(fields[3], "month", 1, 12, None)?,
        days_of_week: parse_cron_field(fields[4], "day-of-week", 0, 7, None)?,
    })
}

fn parse_cron_field(
    field: &str,
    label: &str,
    min: u32,
    max: u32,
    explicit_max: Option<u32>,
) -> Result<CronField, WorkflowServiceError> {
    if field.is_empty() {
        return Err(WorkflowServiceError::InvalidRequest(format!(
            "workflow schedule cron {label} field is empty"
        )));
    }
    if field
        .chars()
        .any(|c| c.is_ascii_alphabetic() || matches!(c, '?' | '#' | 'L' | 'W'))
    {
        return Err(WorkflowServiceError::InvalidRequest(format!(
            "workflow schedule cron {label} field contains an unsupported token"
        )));
    }
    let wildcard = field == "*";
    let mut values = BTreeSet::new();
    for part in field.split(',') {
        parse_cron_part(part, label, min, max, explicit_max, &mut values)?;
    }
    if label == "day-of-week" && values.remove(&7) {
        values.insert(0);
    }
    Ok(CronField { values, wildcard })
}

fn parse_cron_part(
    part: &str,
    label: &str,
    min: u32,
    max: u32,
    explicit_max: Option<u32>,
    values: &mut BTreeSet<u32>,
) -> Result<(), WorkflowServiceError> {
    if part.is_empty() {
        return Err(WorkflowServiceError::InvalidRequest(format!(
            "workflow schedule cron {label} field contains an empty list item"
        )));
    }
    let pieces = part.split('/').collect::<Vec<_>>();
    if pieces.len() > 2 || pieces[0].is_empty() || pieces.get(1).is_some_and(|s| s.is_empty()) {
        return Err(WorkflowServiceError::InvalidRequest(format!(
            "workflow schedule cron {label} field has a malformed step"
        )));
    }
    let step = if pieces.len() == 2 {
        let step = parse_u32(pieces[1], label)?;
        if step == 0 {
            return Err(WorkflowServiceError::InvalidRequest(format!(
                "workflow schedule cron {label} step must be positive"
            )));
        }
        step
    } else {
        1
    };
    let base = pieces[0];
    if label == "day-of-month" && base == "*" && pieces.len() == 2 {
        return Err(WorkflowServiceError::InvalidRequest(
            "day-of-month stepped wildcard is unsupported".to_string(),
        ));
    }
    let (start, end) = if base == "*" {
        (min, max)
    } else {
        let range = base.split('-').collect::<Vec<_>>();
        if range.len() > 2 || range[0].is_empty() || range.get(1).is_some_and(|s| s.is_empty()) {
            return Err(WorkflowServiceError::InvalidRequest(format!(
                "workflow schedule cron {label} field has a malformed range"
            )));
        }
        let start = parse_u32(range[0], label)?;
        let end = if range.len() == 2 {
            parse_u32(range[1], label)?
        } else {
            start
        };
        (start, end)
    };
    if start < min || start > max || end < min || end > max || start > end {
        return Err(WorkflowServiceError::InvalidRequest(format!(
            "workflow schedule cron {label} field is out of range"
        )));
    }
    if explicit_max.is_some_and(|limit| end > limit) && base != "*" {
        return Err(WorkflowServiceError::InvalidRequest(
            "day-of-month values above 28 are unsupported".to_string(),
        ));
    }
    let mut value = start;
    while value <= end {
        values.insert(value);
        match value.checked_add(step) {
            Some(next) => value = next,
            None => break,
        }
    }
    Ok(())
}

fn parse_u32(value: &str, label: &str) -> Result<u32, WorkflowServiceError> {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(WorkflowServiceError::InvalidRequest(format!(
            "workflow schedule cron {label} field must use integers"
        )));
    }
    value.parse::<u32>().map_err(|e| {
        WorkflowServiceError::InvalidRequest(format!(
            "workflow schedule cron {label} field integer is invalid: {e}"
        ))
    })
}

impl ParsedCron {
    fn date_matches(&self, date: NaiveDate) -> bool {
        if !self.months.matches(date.month()) {
            return false;
        }
        let dom = self.days_of_month.matches(date.day());
        let dow = self
            .days_of_week
            .matches(date.weekday().num_days_from_sunday());
        match (self.days_of_month.wildcard, self.days_of_week.wildcard) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }
}

impl CronField {
    fn matches(&self, value: u32) -> bool {
        self.values.contains(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instant(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn calendar_resolves_timezone_gaps_and_ambiguity() {
        let spring = Calendar::parse("30 2 * * *", "America/New_York").unwrap();
        assert_eq!(
            spring.next_after(instant("2026-03-08T06:00:00Z")).unwrap(),
            instant("2026-03-08T07:00:00Z")
        );
        let fall = Calendar::parse("30 1 * * *", "America/New_York").unwrap();
        let first = fall.next_after(instant("2026-11-01T04:00:00Z")).unwrap();
        assert_eq!(first, instant("2026-11-01T05:30:00Z"));
        assert_eq!(
            fall.next_after(first).unwrap(),
            instant("2026-11-02T06:30:00Z")
        );
    }

    #[test]
    fn calendar_rejects_invalid_input_before_scheduling() {
        assert!(Calendar::parse("* * * * *", "Unknown/Zone").is_err());
        assert!(Calendar::parse("* * * * * *", "UTC").is_err());
        assert!(Calendar::parse("*/0 * * * *", "UTC").is_err());
        assert!(Calendar::parse("90 * * * *", "UTC").is_err());
        assert_eq!(
            Calendar::parse("@hourly", "UTC").unwrap().expression(),
            "0 * * * *"
        );
    }
}
