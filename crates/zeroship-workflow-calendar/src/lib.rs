//! Workflow schedule definitions and deterministic calendar calculations.
//!
//! Callers supply the observation time and deployment anchor. This crate owns
//! no clock, persistence, scheduling loop or execution runtime.

mod calendar;
mod timing;

pub use calendar::Calendar;
pub use timing::{IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleTiming};

/// Identity of the calculation semantics and bundled timezone definitions.
///
/// Persist this with prepared schedule metadata and compare before extending
/// its frontier. Calendar semantic changes require a new interpretation version.
#[must_use]
pub fn interpretation() -> String {
    format!("calendar-v1:{}", chrono_tz::IANA_TZDB_VERSION)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CalendarError {
    #[error("{0}")]
    InvalidSchedule(String),
}
