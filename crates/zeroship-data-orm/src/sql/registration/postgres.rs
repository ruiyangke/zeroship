use super::SqlStorageCodecs;
use crate::{
    sql::{compiler::CompileError, statement::StorageType},
    value::Value,
};

#[derive(Clone, Copy)]
pub(super) struct PostgresCodecs;

impl SqlStorageCodecs for PostgresCodecs {
    fn storage_type(&self, definition: &Value) -> Result<StorageType, CompileError> {
        if crate::sql::descriptors::is_encrypted(definition) {
            return Ok(StorageType::Bytes);
        }
        Ok(match definition["type"].as_str() {
            Some("string" | "text" | "id" | "ref" | "calendarDate") => StorageType::Text,
            Some("boolean" | "bool") => StorageType::Boolean,
            Some("integer" | "int" | "bigint" | "bigInt") => StorageType::Integer,
            Some("number" | "float" | "double") => StorageType::Real,
            Some("decimal") => StorageType::Decimal,
            Some("bytes") => StorageType::Bytes,
            Some("date" | "timestamp" | "timestamptz") => StorageType::Timestamp,
            Some("json" | "object" | "array" | "union") => StorageType::Json,
            Some("vector") => StorageType::Vector,
            Some("geoPoint") => StorageType::GeoPoint,
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
            (StorageType::Timestamp, value) => Value::Timestamp(
                crate::sql::temporal::timestamp_millis(&value).ok_or_else(invalid_timestamp)?,
            ),
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
            (StorageType::Json, Value::Json(encoded)) => {
                serde_json::from_str(&encoded).map_err(|_| invalid_json())
            }
            (_, value) => Ok(value),
        }
    }
}

fn unsupported_type() -> CompileError {
    CompileError::InvalidStatement("descriptor has no supported PostgreSQL storage type".into())
}

fn invalid_timestamp() -> CompileError {
    CompileError::InvalidStatement("invalid PostgreSQL timestamp storage value".into())
}

fn invalid_json() -> CompileError {
    CompileError::InvalidStatement("invalid PostgreSQL JSON storage value".into())
}
