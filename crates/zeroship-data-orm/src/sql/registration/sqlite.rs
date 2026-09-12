use super::SqlStorageCodecs;
use crate::{
    sql::{compiler::CompileError, statement::StorageType},
    value::Value,
};

#[derive(Clone, Copy)]
pub(super) struct SqliteCodecs;

impl SqlStorageCodecs for SqliteCodecs {
    fn storage_type(&self, definition: &Value) -> Result<StorageType, CompileError> {
        if crate::sql::descriptors::is_encrypted(definition) {
            return Ok(StorageType::Bytes);
        }
        if let Some(decimal) = crate::sql::decimal::storage(definition)? {
            return Ok(StorageType::ExactDecimal(decimal));
        }
        Ok(match definition["type"].as_str() {
            Some("string" | "text" | "id" | "ref" | "calendarDate") => StorageType::Text,
            Some("boolean" | "bool" | "integer" | "int" | "bigint" | "bigInt") => {
                StorageType::Integer
            }
            Some("number" | "float" | "double") => StorageType::Real,
            Some("bytes") => StorageType::Bytes,
            Some("vector") => StorageType::Vector,
            Some("geoPoint") => StorageType::GeoPoint,
            Some("date" | "timestamp" | "timestamptz") => StorageType::Timestamp,
            Some("json" | "object" | "array" | "union") => StorageType::Json,
            _ => return Err(unsupported_type()),
        })
    }

    fn encode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
        if storage == StorageType::Json && value.is_null() {
            return Ok(Value::Json("null".into()));
        }
        if value.is_null() {
            return Ok(value);
        }
        Ok(match (storage, value) {
            (StorageType::Timestamp, value) => {
                let millis =
                    crate::sql::temporal::timestamp_millis(&value).ok_or_else(invalid_timestamp)?;
                Value::String(
                    crate::sql::temporal::format_timestamp_millis(millis)
                        .expect("validated portable timestamp"),
                )
            }
            (StorageType::Integer, Value::Bool(value)) => Value::from(i64::from(value)),
            (StorageType::Vector, Value::Bytes(bytes)) => {
                decode_vector_blob(&bytes)?;
                Value::Bytes(bytes)
            }
            (StorageType::Vector, Value::Array(values)) => {
                let values = values
                    .into_iter()
                    .map(|value| {
                        let value = value.as_f64().ok_or_else(invalid_vector)? as f32;
                        value
                            .is_finite()
                            .then_some(value)
                            .ok_or_else(invalid_vector)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if values.is_empty() {
                    return Err(invalid_vector());
                }
                Value::Bytes(crate::sql::sqlite_values::vec_to_le_bytes(&values))
            }
            (StorageType::GeoPoint, Value::Bytes(bytes)) => {
                decode_point_blob(&bytes)?;
                Value::Bytes(bytes)
            }
            (StorageType::GeoPoint, Value::Object(point)) => {
                let lat = point
                    .get("lat")
                    .and_then(Value::as_f64)
                    .filter(|value| (-90.0..=90.0).contains(value))
                    .ok_or_else(invalid_point)?;
                let lng = point
                    .get("lng")
                    .and_then(Value::as_f64)
                    .filter(|value| (-180.0..=180.0).contains(value))
                    .ok_or_else(invalid_point)?;
                Value::Bytes(crate::sql::sqlite_values::point_to_blob(
                    crate::sql::descriptors::GeoPoint { lat, lng },
                ))
            }
            (StorageType::Json, value)
                if !matches!(value, Value::Json(_) | Value::Array(_) | Value::Object(_)) =>
            {
                Value::Json(value.to_string())
            }
            (_, value) => value,
        })
    }

    fn decode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
        match (storage, value) {
            (StorageType::Json, Value::String(encoded) | Value::Json(encoded)) => {
                serde_json::from_str(&encoded).map_err(|_| invalid_json())
            }
            (StorageType::Vector, Value::Bytes(bytes)) => {
                decode_vector_blob(&bytes).map(Value::Array)
            }
            (StorageType::GeoPoint, Value::Bytes(bytes)) => decode_point_blob(&bytes),
            (StorageType::ExactDecimal(_), Value::String(encoded))
                if crate::sql::decimal::valid(&encoded) =>
            {
                Ok(Value::Decimal(encoded))
            }
            (_, value) => Ok(value),
        }
    }
}

fn decode_vector_blob(bytes: &[u8]) -> Result<Vec<Value>, CompileError> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(size_of::<f32>()) {
        return Err(invalid_vector());
    }
    bytes
        .chunks_exact(size_of::<f32>())
        .map(|bytes| {
            let value = f32::from_le_bytes(bytes.try_into().expect("vector chunk"));
            Value::try_from(f64::from(value)).map_err(|_| invalid_vector())
        })
        .collect()
}

fn decode_point_blob(bytes: &[u8]) -> Result<Value, CompileError> {
    if bytes.len() != size_of::<f64>() * 2 {
        return Err(invalid_point());
    }
    let [lat, lng] = bytes
        .chunks_exact(size_of::<f64>())
        .map(|bytes| f64::from_le_bytes(bytes.try_into().expect("coordinate bytes")))
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| invalid_point())?;
    if !lat.is_finite()
        || !lng.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lng)
    {
        return Err(invalid_point());
    }
    Ok(crate::value!({"lat":lat,"lng":lng}))
}

fn unsupported_type() -> CompileError {
    CompileError::InvalidStatement("descriptor has no supported SQLite storage type".into())
}

fn invalid_timestamp() -> CompileError {
    CompileError::InvalidStatement("invalid SQLite timestamp storage value".into())
}

fn invalid_json() -> CompileError {
    CompileError::InvalidStatement("invalid SQLite JSON storage value".into())
}

fn invalid_vector() -> CompileError {
    CompileError::InvalidStatement("invalid SQLite vector storage value".into())
}

fn invalid_point() -> CompileError {
    CompileError::InvalidStatement("invalid SQLite geographic storage value".into())
}
