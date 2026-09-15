use super::DbError;
use std::time::Duration;

/// A timestamp evaluated by the database when its statement executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampExpr {
    offset_millis: i64,
}

impl TimestampExpr {
    /// The database wall clock, independent of when a transaction began.
    #[must_use]
    pub const fn database_now() -> Self {
        Self { offset_millis: 0 }
    }

    /// Add a duration without discarding sub-millisecond precision.
    pub fn plus(self, duration: Duration) -> Result<Self, DbError> {
        self.shift(duration, false)
    }

    /// Subtract a duration without discarding sub-millisecond precision.
    pub fn minus(self, duration: Duration) -> Result<Self, DbError> {
        self.shift(duration, true)
    }

    fn shift(self, duration: Duration, subtract: bool) -> Result<Self, DbError> {
        let invalid = || {
            DbError::validation(
                "invalid_timestamp_expression",
                "timestamp offset must fit the portable calendar and use whole milliseconds",
            )
        };
        if !duration.subsec_nanos().is_multiple_of(1_000_000) {
            return Err(invalid());
        }
        let millis = i64::try_from(duration.as_millis()).map_err(|_| invalid())?;
        let offset_millis = if subtract {
            self.offset_millis.checked_sub(millis)
        } else {
            self.offset_millis.checked_add(millis)
        }
        .ok_or_else(invalid)?;
        if !crate::sql::temporal::is_timestamp_offset_millis(offset_millis) {
            return Err(invalid());
        }
        Ok(Self { offset_millis })
    }

    pub(crate) const fn offset_millis(self) -> i64 {
        self.offset_millis
    }
}
