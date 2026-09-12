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
        Ok(match definition["type"].as_str() {
            Some("string" | "text" | "id" | "calendarDate") => StorageType::Text,
            Some("boolean" | "bool" | "integer" | "int" | "bigint" | "bigInt") => {
                StorageType::Integer
            }
            Some("number" | "float" | "double") => StorageType::Real,
            Some("decimal") => StorageType::Decimal,
            Some("bytes" | "vector" | "geoPoint") => StorageType::Bytes,
            Some("date" | "timestamp" | "timestamptz") => StorageType::Timestamp,
            Some("json" | "object" | "array" | "union") => StorageType::Json,
            _ => return Err(unsupported_type()),
        })
    }

    fn encode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
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
            (StorageType::Json, value)
                if !matches!(value, Value::Json(_) | Value::Array(_) | Value::Object(_)) =>
            {
                Value::Json(value.to_string())
            }
            (_, value) => value,
        })
    }

    fn decode(&self, _: StorageType, value: Value) -> Result<Value, CompileError> {
        Ok(value)
    }
}

fn unsupported_type() -> CompileError {
    CompileError::InvalidStatement("descriptor has no supported SQLite storage type".into())
}

fn invalid_timestamp() -> CompileError {
    CompileError::InvalidStatement("invalid SQLite timestamp storage value".into())
}
