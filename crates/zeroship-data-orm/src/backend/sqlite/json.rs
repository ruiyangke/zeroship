//! Schema-free JSON equality required by the SQLite SQL renderer.
use rusqlite::{Connection, functions::FunctionFlags};

pub(super) fn register(connection: &Connection) -> rusqlite::Result<()> {
    connection.create_scalar_function(
        "zeroship_json_equal",
        2,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| {
            let left: String = context.get(0)?;
            let right = context.get_or_create_aux(1, |value| {
                zeroship_data_sql::json::comparison_key(value.as_str()?)
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
            })?;
            let left = zeroship_data_sql::json::comparison_key(&left)
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
            Ok(left == *right)
        },
    )
}
