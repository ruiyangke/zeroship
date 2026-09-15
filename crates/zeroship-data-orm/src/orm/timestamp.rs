use super::DbError;
use std::time::Duration;

/// A timestamp evaluated by the database when its statement executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampExpr {
    offset_micros: i64,
}

impl TimestampExpr {
    /// The database wall clock, independent of when a transaction began.
    #[must_use]
    pub const fn database_now() -> Self {
        Self { offset_micros: 0 }
    }

    /// Add a duration of whole microseconds.
    ///
    /// # Errors
    /// [`DbError::ValidationFailed`] when the duration carries a
    /// sub-microsecond part or the shifted instant leaves the portable
    /// calendar. A backend that stores milliseconds refuses a sub-millisecond
    /// offset separately, when the statement is compiled for it.
    pub fn plus(self, duration: Duration) -> Result<Self, DbError> {
        self.shift(duration, false)
    }

    /// Subtract a duration of whole microseconds.
    ///
    /// # Errors
    /// The peer of [`TimestampExpr::plus`].
    pub fn minus(self, duration: Duration) -> Result<Self, DbError> {
        self.shift(duration, true)
    }

    fn shift(self, duration: Duration, subtract: bool) -> Result<Self, DbError> {
        let invalid = || {
            DbError::validation(
                "invalid_timestamp_expression",
                "timestamp offset must fit the portable calendar and use whole microseconds",
            )
        };
        if !duration.subsec_nanos().is_multiple_of(1_000) {
            return Err(invalid());
        }
        let micros = i64::try_from(duration.as_micros()).map_err(|_| invalid())?;
        let offset_micros = if subtract {
            self.offset_micros.checked_sub(micros)
        } else {
            self.offset_micros.checked_add(micros)
        }
        .ok_or_else(invalid)?;
        if !crate::sql::temporal::is_timestamp_offset_micros(offset_micros) {
            return Err(invalid());
        }
        Ok(Self { offset_micros })
    }

    pub(crate) const fn offset_micros(self) -> i64 {
        self.offset_micros
    }
}
