use super::SqlStorageCodecs;
use crate::schema::{ColumnSchema, LogicalType};
use crate::{
    sql::{compiler::CompileError, statement::StorageType},
    value::Value,
};

#[derive(Clone, Copy)]
pub(super) struct PostgresCodecs;

impl SqlStorageCodecs for PostgresCodecs {
    fn storage_type(&self, definition: &ColumnSchema) -> Result<StorageType, CompileError> {
        if crate::sql::descriptors::is_encrypted(definition) {
            return Ok(StorageType::Bytes);
        }
        if let Some(decimal) = crate::sql::decimal::storage(definition)? {
            return Ok(StorageType::ExactDecimal(decimal));
        }
        Ok(match definition.logical_type {
            LogicalType::Text | LogicalType::CalendarDate => StorageType::Text,
            LogicalType::Boolean => StorageType::Boolean,
            LogicalType::Integer | LogicalType::BigInt => StorageType::Integer,
            LogicalType::Number => StorageType::Real,
            LogicalType::Bytes => StorageType::Bytes,
            LogicalType::Timestamp => StorageType::Timestamp,
            LogicalType::Json | LogicalType::Object | LogicalType::Array | LogicalType::Union => {
                StorageType::Json
            }
            LogicalType::Vector => StorageType::Vector,
            LogicalType::GeoPoint => StorageType::GeoPoint,
            LogicalType::Time | LogicalType::Enum | LogicalType::Literal => {
                return Err(unsupported_type())
            }
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
            (StorageType::ExactDecimal(_), Value::String(encoded))
                if crate::sql::decimal::valid(&encoded) =>
            {
                Ok(Value::Decimal(encoded))
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
