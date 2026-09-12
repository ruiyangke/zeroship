use crate::sql::{
    decimal::{self, Arithmetic},
    statement::DecimalStorage,
};
use rusqlite::{functions::FunctionFlags, types::ValueRef, Connection};

const FLAGS: FunctionFlags = FunctionFlags::SQLITE_UTF8
    .union(FunctionFlags::SQLITE_DETERMINISTIC)
    .union(FunctionFlags::SQLITE_INNOCUOUS);

pub(super) fn register(connection: &Connection) -> rusqlite::Result<()> {
    connection.create_scalar_function("zeroship_decimal_equal", 2, FLAGS, |context| {
        let (Some(left), Some(right)) = (text(context, 0)?, text(context, 1)?) else {
            return Ok(None);
        };
        decimal::equivalent(left, right)
            .map(Some)
            .map_err(user_error)
    })?;
    connection.create_scalar_function("zeroship_decimal_quantize", 3, FLAGS, |context| {
        let Some(value) = text(context, 0)? else {
            return Ok(None);
        };
        decimal::quantize(value, storage(context, 1, 2)?)
            .map(Some)
            .map_err(user_error)
    })?;
    for (name, operation) in [
        ("zeroship_decimal_add", Arithmetic::Add),
        ("zeroship_decimal_subtract", Arithmetic::Subtract),
        ("zeroship_decimal_multiply", Arithmetic::Multiply),
    ] {
        connection.create_scalar_function(name, 4, FLAGS, move |context| {
            let (Some(left), Some(right)) = (text(context, 0)?, text(context, 1)?) else {
                return Ok(None);
            };
            decimal::arithmetic(left, right, operation, storage(context, 2, 3)?)
                .map(Some)
                .map_err(user_error)
        })?;
    }
    Ok(())
}

fn text<'a>(
    context: &'a rusqlite::functions::Context<'a>,
    index: usize,
) -> rusqlite::Result<Option<&'a str>> {
    match context.get_raw(index) {
        ValueRef::Null => Ok(None),
        value => Ok(Some(value.as_str()?)),
    }
}

fn storage(
    context: &rusqlite::functions::Context<'_>,
    precision: usize,
    scale: usize,
) -> rusqlite::Result<DecimalStorage> {
    let precision = u64::try_from(context.get::<i64>(precision)?).map_err(user_error)?;
    let scale = u64::try_from(context.get::<i64>(scale)?).map_err(user_error)?;
    DecimalStorage::new(precision, scale).map_err(user_error)
}

fn user_error(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_functions_compare_and_mutate_exact_values() {
        let connection = Connection::open_in_memory().unwrap();
        register(&connection).unwrap();
        let equal: bool = connection
            .query_row("SELECT zeroship_decimal_equal('1.0', '1.00')", [], |row| {
                row.get(0)
            })
            .unwrap();
        let value: String = connection
            .query_row(
                "SELECT zeroship_decimal_add('9007199254740993.00', '0.01', 30, 2)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(equal);
        assert_eq!(value, "9007199254740993.01");
    }
}
