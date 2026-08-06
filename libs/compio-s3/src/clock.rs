//! Injectable UTC clock and `SigV4` date formatting.
//!
//! `SigV4` requires two formats derived from the *same* instant:
//!
//! - `x-amz-date`: ISO-8601 basic, `YYYYMMDD'T'HHMMSS'Z'`.
//! - credential-scope date: `YYYYMMDD`.
//!
//! If these two diverge (e.g. a day-boundary race where one is computed
//! before midnight and the other after) the signature is rejected by S3.
//! A single `SigningTime` snapshot derives both from one instant so they
//! can never disagree. Tests inject a fixed instant so signatures are
//! deterministic and reproducible against the AWS test vectors.

use std::time::SystemTime;

use time::OffsetDateTime;

/// An injectable UTC clock. Production uses [`SystemClock`]; tests use a
/// fixed clock so `SigV4` output is deterministic.
pub trait Clock: std::fmt::Debug + Send + Sync {
    /// Current wall-clock instant.
    fn now(&self) -> SystemTime;
}

/// Real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A fixed clock for deterministic tests.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

// The two SigV4 date formats are produced directly from the
// `OffsetDateTime` UTC components, avoiding the `time/macros` feature
// (and its proc-macro dependencies). Both derive from the SAME instant
// so the scope date can never diverge from `x-amz-date`.
fn format_amz_date(odt: OffsetDateTime) -> String {
    // `YYYYMMDD'T'HHMMSS'Z'`
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        odt.year(),
        u8::from(odt.month()),
        odt.day(),
        odt.hour(),
        odt.minute(),
        odt.second(),
    )
}

fn format_scope_date(odt: OffsetDateTime) -> String {
    // `YYYYMMDD`
    format!(
        "{:04}{:02}{:02}",
        odt.year(),
        u8::from(odt.month()),
        odt.day(),
    )
}

/// A single instant rendered into both `SigV4` date forms. Both strings are
/// derived from the same `OffsetDateTime` so the scope date can never drift
/// from `x-amz-date`.
#[derive(Debug, Clone)]
pub struct SigningTime {
    amz_date: String,
    scope_date: String,
}

impl SigningTime {
    /// Snapshot a clock into the two `SigV4` date strings.
    pub fn from_clock(clock: &dyn Clock) -> Self {
        Self::from_system_time(clock.now())
    }

    /// Snapshot a concrete `SystemTime` (UTC).
    #[must_use] 
    pub fn from_system_time(t: SystemTime) -> Self {
        // `SystemTime -> OffsetDateTime` is UTC.
        let odt: OffsetDateTime = t.into();
        Self {
            amz_date: format_amz_date(odt),
            scope_date: format_scope_date(odt),
        }
    }

    /// `x-amz-date` header value, `YYYYMMDD'T'HHMMSS'Z'`.
    #[must_use] 
    pub fn amz_date(&self) -> &str {
        &self.amz_date
    }

    /// Credential-scope date, `YYYYMMDD`.
    #[must_use] 
    pub fn scope_date(&self) -> &str {
        &self.scope_date
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 2015-08-30T12:36:00Z — the instant used by the AWS `SigV4` test-suite
    /// (`20150830T123600Z`, scope `20150830`).
    fn aws_suite_instant() -> SystemTime {
        // 1440938160 = 2015-08-30T12:36:00Z
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160)
    }

    #[test]
    fn formats_match_aws_test_suite_instant() {
        let st = SigningTime::from_system_time(aws_suite_instant());
        assert_eq!(st.amz_date(), "20150830T123600Z");
        assert_eq!(st.scope_date(), "20150830");
    }

    #[test]
    fn scope_date_cannot_diverge_across_day_boundary() {
        // 2023-01-01T00:00:00Z exactly.
        let midnight = SystemTime::UNIX_EPOCH + Duration::from_secs(1_672_531_200);
        let just_before = midnight - Duration::from_secs(1);

        let at = SigningTime::from_system_time(midnight);
        assert_eq!(at.amz_date(), "20230101T000000Z");
        assert_eq!(at.scope_date(), "20230101");

        let before = SigningTime::from_system_time(just_before);
        assert_eq!(before.amz_date(), "20221231T235959Z");
        // The scope date is derived from the SAME instant as amz_date, so it
        // is 20221231, never 20230101.
        assert_eq!(before.scope_date(), "20221231");
        assert_eq!(&before.amz_date()[..8], before.scope_date());
    }

    #[test]
    fn fixed_clock_round_trips() {
        let clock = FixedClock(aws_suite_instant());
        let st = SigningTime::from_clock(&clock);
        assert_eq!(st.amz_date(), "20150830T123600Z");
    }
}
