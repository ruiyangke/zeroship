//! Convert SQLite actor results into native records.

use super::session::{TypedCell, TypedRows};
use crate::error::DbError;
use zeroship_data_sql::value::{Map, Value};

/// Preserve storage types and reject values the native contract cannot express.
pub fn typed_rows_to_values(rows: &TypedRows) -> Result<Vec<Value>, DbError> {
    rows.rows
        .iter()
        .map(|row| typed_row_to_record(&rows.columns, row).map(Value::Object))
        .collect()
}

pub(crate) fn typed_row_to_record(
    columns: &[String],
    row: &[TypedCell],
) -> Result<Map<String, Value>, DbError> {
    if columns.len() != row.len() {
        return Err(DbError::row_decode(
            "<row>",
            "column and value counts differ",
        ));
    }
    let mut object = Map::with_capacity(columns.len());
    for (name, cell) in columns.iter().zip(row) {
        let value = match cell {
            TypedCell::Null => Value::Null,
            TypedCell::Integer(value) => Value::from(*value),
            TypedCell::Real(value) => Value::try_from(*value)
                .map_err(|_| DbError::row_decode(name, "non-finite numbers are unsupported"))?,
            TypedCell::Text(value) => Value::String(value.clone()),
            TypedCell::Blob(bytes) => Value::Bytes(bytes.clone()),
        };
        object.insert(name.clone(), value);
    }
    Ok(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_rows_reject_mismatched_shapes_and_nonfinite_cells() {
        let columns = vec!["item".into()];
        for row in [
            vec![],
            vec![TypedCell::Null, TypedCell::Null],
            vec![TypedCell::Real(f64::NAN)],
        ] {
            assert!(typed_row_to_record(&columns, &row).is_err());
        }
        let rows = TypedRows {
            columns,
            rows: vec![vec![TypedCell::Null], vec![TypedCell::Real(f64::INFINITY)]],
        };
        assert!(typed_rows_to_values(&rows).is_err());
    }
}
