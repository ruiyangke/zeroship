use super::SqlStorageCodecs;
use crate::schema::{ColumnSchema, LogicalType};
use crate::{
    sql::{
        compiler::CompileError,
        statement::{ArrayElement, StorageType},
    },
    value::Value,
};

#[derive(Clone, Copy)]
pub(super) struct PostgresCodecs;

impl SqlStorageCodecs for PostgresCodecs {
    fn storage_type(&self, definition: &ColumnSchema) -> Result<StorageType, CompileError> {
        if crate::sql::descriptors::is_encrypted(definition) {
            return Ok(StorageType::Bytes);
        }
        if definition.has_native_array_storage() {
            return match definition.items {
                Some(LogicalType::Text) => Ok(StorageType::Array(ArrayElement::Text)),
                _ => Err(unsupported_type()),
            };
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
            (StorageType::Array(ArrayElement::Text), Value::Json(encoded)) => {
                text_array(serde_json::from_str(&encoded).map_err(|_| invalid_array())?)?
            }
            (StorageType::Array(ArrayElement::Text), value) => text_array(value)?,
            // An array must not reach the driver as a structure: it would then
            // bind to a native array column that the descriptor declares as JSON.
            (StorageType::Json, value @ Value::Array(_)) => {
                Value::Json(serde_json::to_string(&value).map_err(|_| invalid_json())?)
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
            (StorageType::Json, Value::Json(encoded)) => {
                serde_json::from_str(&encoded).map_err(|_| invalid_json())
            }
            (StorageType::ExactDecimal(_), Value::String(encoded))
                if crate::sql::decimal::valid(&encoded) =>
            {
                Ok(Value::Decimal(encoded))
            }
            (StorageType::Array(_), value @ (Value::Null | Value::Array(_))) => Ok(value),
            (StorageType::Array(_), _) => Err(invalid_array()),
            (_, value) => Ok(value),
        }
    }
}

/// A native text array binds as a whole; SQL NULL elements are not representable
/// as ORM text values, and PostgreSQL text cannot hold NUL.
fn text_array(value: Value) -> Result<Value, CompileError> {
    match value {
        Value::Array(values)
            if values
                .iter()
                .all(|value| matches!(value, Value::String(text) if !text.contains('\0'))) =>
        {
            Ok(Value::Array(values))
        }
        _ => Err(invalid_array()),
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

fn invalid_array() -> CompileError {
    CompileError::InvalidStatement("invalid PostgreSQL text array storage value".into())
}
