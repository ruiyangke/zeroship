//! Schema-free JSON equality required by the SQLite SQL renderer.
use rusqlite::{functions::FunctionFlags, Connection};

pub(super) fn register(connection: &Connection) -> rusqlite::Result<()> {
    connection.create_scalar_function(
        "zeroship_json_equal",
        2,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let left = context.get_raw(0);
            if matches!(left, rusqlite::types::ValueRef::Null) {
                return Ok(None);
            }
            let right = context.get_or_create_aux(1, |value| {
                if matches!(value, rusqlite::types::ValueRef::Null) {
                    return Ok(None);
                }
                crate::sql::json::comparison_key(value.as_str()?)
                    .map(Some)
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
            })?;
            let Some(right) = right.as_ref() else {
                return Ok(None);
            };
            let left = left
                .as_str()
                .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
            let left = crate::sql::json::comparison_key(left)
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
            Ok(Some(left == *right))
        },
    )
}
