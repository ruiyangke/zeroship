//! Convert SQLite actor results into native records.

use zeroship_data_query_builder::value::Value;

use super::session::{TypedCell, TypedRows};

/// Preserve native storage types while assembling record fields.
pub fn typed_rows_to_values(rows: &TypedRows) -> Vec<Value> {
    rows.rows
        .iter()
        .map(|row| Value::Object(typed_row_to_record(&rows.columns, row)))
        .collect()
}

pub(crate) fn typed_row_to_record(
    columns: &[String],
    row: &[TypedCell],
) -> zeroship_data_query_builder::value::Map<String, Value> {
    let mut obj = zeroship_data_query_builder::value::Map::new();
    for (idx, name) in columns.iter().enumerate() {
        let value = row.get(idx).map(typed_cell_to_value).unwrap_or(Value::Null);
        obj.insert(name.clone(), value);
    }
    obj
}

fn typed_cell_to_value(cell: &TypedCell) -> Value {
    match cell {
        TypedCell::Null => Value::Null,
        TypedCell::Integer(n) => {
            Value::Number(zeroship_data_query_builder::value::Number::from(*n))
        }
        TypedCell::Real(f) => zeroship_data_query_builder::value::Number::from_f64(*f)
            .map_or(Value::Null, Value::Number),
        TypedCell::Text(s) => Value::String(s.clone()),
        TypedCell::Blob(bytes) => Value::Bytes(bytes.clone()),
    }
}
