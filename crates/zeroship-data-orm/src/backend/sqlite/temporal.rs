//! Exact timestamp arithmetic over values supplied by SQLite's database clock.
use rusqlite::{Connection, functions::FunctionFlags};

/// The result code `zeroship_timestamp_add` raises when the shifted instant
/// falls outside the portable calendar, classified by
/// [`super::error::from_sqlite`]. The engine itself never raises
/// `SQLITE_CONSTRAINT_FUNCTION`; it is reserved for application-defined
/// functions. The code is raised without a message because rusqlite attaches a
/// message after the code, which records the failure as `SQLITE_ERROR`. For the
/// same reason, function errors that carry a message never arrive with this
/// code.
pub(super) const OUTSIDE_CALENDAR: i32 = rusqlite::ffi::SQLITE_CONSTRAINT_FUNCTION;

pub(super) fn register(connection: &Connection) -> rusqlite::Result<()> {
    connection.create_scalar_function(
        "zeroship_timestamp_add",
        2,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let timestamp: String = context.get(0)?;
            let offset: i64 = context.get(1)?;
            let clock =
                crate::sql::temporal::parse_timestamp_millis(&timestamp).ok_or_else(|| {
                    rusqlite::Error::UserFunctionError(
                        "database clock is not a portable timestamp".into(),
                    )
                })?;
            clock
                .checked_add(offset)
                .and_then(crate::sql::temporal::format_timestamp_millis)
                .ok_or_else(|| {
                    rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(OUTSIDE_CALENDAR),
                        None,
                    )
                })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_arithmetic_preserves_fractional_offsets_and_refuses_invalid_results() {
        let directory = tempfile::tempdir().unwrap();
        let connection = Connection::open(directory.path().join("temporal.sqlite")).unwrap();
        register(&connection).unwrap();
        let shift = |timestamp: &str, offset: i64| {
            connection.query_row(
                "SELECT zeroship_timestamp_add(?1, ?2)",
                (timestamp, offset),
                |row| row.get::<_, String>(0),
            )
        };
        assert_eq!(
            shift("1970-01-01T00:00:00.000Z", -1).unwrap(),
            "1969-12-31T23:59:59.999Z"
        );
        assert_eq!(
            shift("1969-12-31T23:59:59.999Z", 1).unwrap(),
            "1970-01-01T00:00:00.000Z"
        );
        for (timestamp, offset) in [
            ("9999-12-31T23:59:59.999Z", 1),
            ("0001-01-01T00:00:00.000Z", -1),
        ] {
            let error = super::super::error::from_sqlite(shift(timestamp, offset).unwrap_err());
            assert!(
                matches!(
                    error,
                    zeroship_data_orm::error::DbError::ValidationFailed {
                        code: "invalid_timestamp_expression",
                        ..
                    }
                ),
                "{timestamp} {offset:+}: {error:?}"
            );
        }
        let error = super::super::error::from_sqlite(shift("invalid", 0).unwrap_err());
        assert!(
            !matches!(
                error,
                zeroship_data_orm::error::DbError::ValidationFailed { .. }
            ),
            "an unreadable clock is not a caller's out-of-calendar offset: {error:?}"
        );
    }
}
