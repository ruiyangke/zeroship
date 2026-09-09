//! Native Rust model mapping. Values move between model fields and records;
//! neither direction constructs a JSON representation.
use std::marker::PhantomData;
use zeroship_data_core::error::DbError;
use zeroship_data_query_builder::value::{Record, Value};

pub trait EncodeRecord {
    fn into_record(self) -> Record;
}
impl EncodeRecord for Record {
    fn into_record(self) -> Record {
        self
    }
}

pub trait DecodeValue: Sized {
    fn decode(value: Value) -> Result<Self, DbError>;
}
fn mismatch(expected: &str) -> DbError {
    DbError::validation("model_decode_failed", format!("expected {expected}"))
}
impl DecodeValue for String {
    fn decode(value: Value) -> Result<Self, DbError> {
        match value {
            Value::String(v) => Ok(v),
            _ => Err(mismatch("text")),
        }
    }
}
impl DecodeValue for Vec<u8> {
    fn decode(value: Value) -> Result<Self, DbError> {
        match value {
            Value::Bytes(v) => Ok(v),
            _ => Err(mismatch("bytes")),
        }
    }
}
impl DecodeValue for bool {
    fn decode(value: Value) -> Result<Self, DbError> {
        match value {
            Value::Bool(v) => Ok(v),
            _ => Err(mismatch("boolean")),
        }
    }
}
impl DecodeValue for i64 {
    fn decode(value: Value) -> Result<Self, DbError> {
        value.as_i64().ok_or_else(|| mismatch("integer"))
    }
}
impl DecodeValue for f64 {
    fn decode(value: Value) -> Result<Self, DbError> {
        value.as_f64().ok_or_else(|| mismatch("number"))
    }
}
impl<T: DecodeValue> DecodeValue for Option<T> {
    fn decode(value: Value) -> Result<Self, DbError> {
        if value.is_null() {
            Ok(None)
        } else {
            T::decode(value).map(Some)
        }
    }
}
impl DecodeValue for Value {
    fn decode(value: Value) -> Result<Self, DbError> {
        Ok(value)
    }
}

/// A protected result row. Taking a field moves its allocation into the model.
#[derive(Debug)]
pub struct Row(Record);
impl Row {
    pub(crate) fn new(fields: Record) -> Self {
        Self(fields)
    }
    pub fn take<T: DecodeValue>(&mut self, name: &str) -> Result<T, DbError> {
        let value = self.0.swap_remove(name).ok_or_else(|| {
            DbError::validation("model_decode_failed", format!("missing field '{name}'"))
        })?;
        T::decode(value)
    }
}

/// A declared field tied to its model and Rust input type.
#[derive(Debug)]
pub struct Field<M, T> {
    name: &'static str,
    marker: PhantomData<fn(M) -> T>,
}
impl<M, T> Copy for Field<M, T> {}
impl<M, T> Clone for Field<M, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<M, T> Field<M, T> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }
    pub fn try_eq(self, value: T) -> Result<Filter<M>, DbError>
    where
        T: TryInto<Value>,
        T::Error: std::fmt::Display,
    {
        let value = value
            .try_into()
            .map_err(|e| DbError::validation("invalid_value", e.to_string()))?;
        Ok(Filter {
            value: Value::Object([(self.name.into(), value)].into()),
            model: PhantomData,
        })
    }
    pub fn try_set(self, value: T) -> Result<Patch<M>, DbError>
    where
        T: TryInto<Value>,
        T::Error: std::fmt::Display,
    {
        let value = value
            .try_into()
            .map_err(|e| DbError::validation("invalid_value", e.to_string()))?;
        Ok(Patch {
            fields: [(self.name.into(), value)].into(),
            model: PhantomData,
        })
    }
    pub fn eq(self, value: T) -> Filter<M>
    where
        T: Into<Value>,
    {
        Filter {
            value: Value::Object([(self.name.into(), value.into())].into()),
            model: PhantomData,
        }
    }
    pub fn set(self, value: T) -> Patch<M>
    where
        T: Into<Value>,
    {
        Patch {
            fields: [(self.name.into(), value.into())].into(),
            model: PhantomData,
        }
    }
}

/// A filter whose fields belong to the same model.
#[derive(Debug)]
pub struct Filter<M> {
    value: Value,
    model: PhantomData<fn() -> M>,
}
impl<M> Default for Filter<M> {
    fn default() -> Self {
        Self::all()
    }
}
impl<M> Filter<M> {
    pub fn all() -> Self {
        Self {
            value: Value::Object(Record::new()),
            model: PhantomData,
        }
    }
    pub fn and(self, other: Self) -> Self {
        Self {
            value: Value::Object(
                [("$and".into(), Value::Array(vec![self.value, other.value]))].into(),
            ),
            model: PhantomData,
        }
    }
    pub fn or(self, other: Self) -> Self {
        Self {
            value: Value::Object(
                [("$or".into(), Value::Array(vec![self.value, other.value]))].into(),
            ),
            model: PhantomData,
        }
    }
    pub(crate) fn into_value(self) -> Value {
        self.value
    }
}

#[derive(Debug)]
pub struct Patch<M> {
    fields: Record,
    model: PhantomData<fn() -> M>,
}
impl<M> Patch<M> {
    pub fn and(mut self, other: Self) -> Self {
        self.fields.extend(other.fields);
        self
    }
    pub(crate) fn into_value(self) -> Value {
        Value::Object(self.fields)
    }
}

#[derive(Default, Debug)]
pub struct FindOptions {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub include_deleted: bool,
}
impl FindOptions {
    pub(crate) fn into_value(self) -> Value {
        let mut fields = Record::new();
        if let Some(limit) = self.limit {
            fields.insert("limit".into(), limit.into());
        }
        if let Some(offset) = self.offset {
            fields.insert("offset".into(), offset.into());
        }
        if self.include_deleted {
            fields.insert("include_deleted".into(), true.into());
        }
        Value::Object(fields)
    }
}

macro_rules! decode_integer {
    ($($ty:ty),*) => { $(impl DecodeValue for $ty {
        fn decode(value: Value) -> Result<Self, DbError> {
            match value {
                Value::Number(n) => n.as_i64().and_then(|v| Self::try_from(v).ok())
                    .or_else(|| n.as_u64().and_then(|v| Self::try_from(v).ok()))
                    .ok_or_else(|| mismatch(stringify!($ty))),
                _ => Err(mismatch(stringify!($ty))),
            }
        }
    })* };
}
decode_integer!(i8, i16, i32, isize, u8, u16, u32, u64, usize);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn row_mapping_moves_buffers_and_rejects_wrong_types() {
        let bytes = vec![1u8, 2, 3];
        let pointer = bytes.as_ptr();
        let text = String::from("owned text");
        let text_pointer = text.as_ptr();
        let mut row = Row::new(
            [
                ("bytes".into(), bytes.into()),
                ("text".into(), text.into()),
                ("too_large".into(), 256.into()),
            ]
            .into(),
        );
        let decoded_bytes = row.take::<Vec<u8>>("bytes").unwrap();
        assert_eq!(decoded_bytes.as_ptr(), pointer);
        let decoded_text = row.take::<String>("text").unwrap();
        assert_eq!(decoded_text.as_ptr(), text_pointer);
        assert!(row.take::<u8>("too_large").is_err());
        assert!(row.take::<String>("missing").is_err());
        assert!(Field::<(), f64>::new("score").try_eq(f64::NAN).is_err());
        assert!(Field::<(), f64>::new("score").try_set(1.25).is_ok());
    }
}
