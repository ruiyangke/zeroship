//! Exact timestamp arithmetic over values supplied by SQLite's database clock.
use rusqlite::{Connection, functions::FunctionFlags};

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
            crate::sql::temporal::parse_timestamp_millis(&timestamp)
                .and_then(|timestamp| timestamp.checked_add(offset))
                .and_then(crate::sql::temporal::format_timestamp_millis)
                .ok_or_else(|| {
                    rusqlite::Error::UserFunctionError(
                        "timestamp expression is outside the portable calendar".into(),
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
        assert!(shift("9999-12-31T23:59:59.999Z", 1).is_err());
        assert!(shift("0001-01-01T00:00:00.000Z", -1).is_err());
        assert!(shift("invalid", 0).is_err());
    }
}
