//! Native codecs selected by generated logical column types.
use super::{DecodeValue, EncodeValue, Value};
use zeroship_data_orm::error::DbError;

/// Logical types carried by migration-derived column metadata.
pub mod sql_types {
    macro_rules! markers {
        ($($name:ident),* $(,)?) => { $(#[derive(Debug)] pub enum $name {})* };
    }
    markers!(
        Text,
        Integer,
        BigInt,
        Number,
        Boolean,
        Bytes,
        Timestamp,
        CalendarDate,
        Time,
        Json,
        Vector,
        GeoPoint
    );
    #[derive(Debug)]
    pub struct Nullable<S>(std::marker::PhantomData<S>);
}
use sql_types::*;

fn invalid(expected: &str) -> DbError {
    DbError::validation("invalid_model_value", format!("expected {expected}"))
}

macro_rules! text_codec {
    ($($sql:ty),* $(,)?) => { $(
        impl EncodeValue<$sql> for String {
            fn encode_value(self) -> Result<Value, DbError> { Ok(Value::String(self)) }
        }
        impl EncodeValue<$sql> for &str {
            fn encode_value(self) -> Result<Value, DbError> { Ok(Value::String(self.into())) }
        }
        impl DecodeValue<$sql> for String {
            fn decode_value(value: Value) -> Result<Self, DbError> {
                match value { Value::String(value) => Ok(value), _ => Err(invalid("text")) }
            }
        }
    )* };
}
text_codec!(Text, CalendarDate, Time);

impl EncodeValue<Bytes> for Vec<u8> {
    fn encode_value(self) -> Result<Value, DbError> {
        Ok(Value::Bytes(self))
    }
}
impl EncodeValue<Bytes> for &[u8] {
    fn encode_value(self) -> Result<Value, DbError> {
        Ok(Value::Bytes(self.to_vec()))
    }
}
impl DecodeValue<Bytes> for Vec<u8> {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        match value {
            Value::Bytes(value) => Ok(value),
            _ => Err(invalid("bytes")),
        }
    }
}
impl EncodeValue<Boolean> for bool {
    fn encode_value(self) -> Result<Value, DbError> {
        Ok(Value::Bool(self))
    }
}
impl DecodeValue<Boolean> for bool {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        match value {
            Value::Bool(value) => Ok(value),
            _ => Err(invalid("boolean")),
        }
    }
}

macro_rules! integer_codec {
    ($sql:ty, $storage:ty, $($rust:ty),* $(,)?) => { $(
        impl EncodeValue<$sql> for $rust {
            fn encode_value(self) -> Result<Value, DbError> {
                let value = <$storage>::try_from(self).map_err(|_| invalid(stringify!($storage)))?;
                Ok(Value::from(value))
            }
        }
        impl DecodeValue<$sql> for $rust {
            fn decode_value(value: Value) -> Result<Self, DbError> {
                value.as_i64().and_then(|v| Self::try_from(v).ok()).ok_or_else(|| invalid(stringify!($rust)))
            }
        }
    )* };
}
integer_codec!(
    Integer, i32, i8, i16, i32, i64, u8, u16, u32, u64, isize, usize
);
integer_codec!(
    BigInt, i64, i8, i16, i32, i64, u8, u16, u32, u64, isize, usize
);

impl EncodeValue<Number> for f64 {
    fn encode_value(self) -> Result<Value, DbError> {
        Value::try_from(self).map_err(|_| invalid("finite number"))
    }
}
impl EncodeValue<Number> for f32 {
    fn encode_value(self) -> Result<Value, DbError> {
        <f64 as EncodeValue<Number>>::encode_value(f64::from(self))
    }
}
impl DecodeValue<Number> for f64 {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        let number = match value {
            Value::Decimal(value) => value.parse().ok(),
            value => value.as_f64(),
        };
        number
            .filter(|v| v.is_finite())
            .ok_or_else(|| invalid("finite number"))
    }
}
impl DecodeValue<Number> for f32 {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        let number = <f64 as DecodeValue<Number>>::decode_value(value)? as Self;
        if number.is_finite() {
            Ok(number)
        } else {
            Err(invalid("finite float"))
        }
    }
}

/// An exact decimal representation when the database returns NUMERIC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decimal(pub String);
impl EncodeValue<Number> for Decimal {
    fn encode_value(self) -> Result<Value, DbError> {
        // Validate JSON's decimal grammar without converting through a float.
        let raw = serde_json::from_str::<&serde_json::value::RawValue>(&self.0)
            .map_err(|_| invalid("decimal"))?;
        if !raw
            .get()
            .starts_with(|c: char| c == '-' || c.is_ascii_digit())
        {
            return Err(invalid("decimal"));
        }
        Ok(Value::Decimal(self.0))
    }
}
impl DecodeValue<Number> for Decimal {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        match value {
            Value::Decimal(value) => Ok(Self(value)),
            Value::Number(value) => Ok(Self(value.to_string())),
            _ => Err(invalid("decimal")),
        }
    }
}

impl EncodeValue<Timestamp> for i64 {
    fn encode_value(self) -> Result<Value, DbError> {
        Ok(Value::Timestamp(self))
    }
}
impl DecodeValue<Timestamp> for i64 {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        match value {
            Value::Timestamp(value) => Ok(value),
            value => value.as_i64().ok_or_else(|| invalid("timestamp")),
        }
    }
}
impl<S, T: EncodeValue<S>> EncodeValue<Nullable<S>> for Option<T> {
    fn encode_value(self) -> Result<Value, DbError> {
        self.map(EncodeValue::encode_value)
            .unwrap_or(Ok(Value::Null))
    }
}
impl<S, T: DecodeValue<S>> DecodeValue<Nullable<S>> for Option<T> {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        if value.is_null() {
            Ok(None)
        } else {
            T::decode_value(value).map(Some)
        }
    }
}

impl EncodeValue<Json> for Value {
    fn encode_value(self) -> Result<Value, DbError> {
        Ok(self)
    }
}
impl DecodeValue<Json> for Value {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        Ok(value)
    }
}
macro_rules! vector_codec {
    ($($rust:ty),* $(,)?) => { $(
        impl EncodeValue<Vector> for Vec<$rust> {
            fn encode_value(self) -> Result<Value, DbError> {
                self.into_iter().map(<$rust as EncodeValue<Number>>::encode_value)
                    .collect::<Result<Vec<_>, _>>().map(Value::Array)
            }
        }
        impl DecodeValue<Vector> for Vec<$rust> {
            fn decode_value(value: Value) -> Result<Self, DbError> {
                let Value::Array(values) = value else { return Err(invalid("vector")); };
                values.into_iter().map(<$rust as DecodeValue<Number>>::decode_value).collect()
            }
        }
    )* };
}
vector_codec!(f32, f64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub lat: f64,
    pub lng: f64,
}
impl EncodeValue<GeoPoint> for Point {
    fn encode_value(self) -> Result<Value, DbError> {
        Ok(Value::Object(
            [
                (
                    "lat".into(),
                    <f64 as EncodeValue<Number>>::encode_value(self.lat)?,
                ),
                (
                    "lng".into(),
                    <f64 as EncodeValue<Number>>::encode_value(self.lng)?,
                ),
            ]
            .into(),
        ))
    }
}
impl DecodeValue<GeoPoint> for Point {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        let Value::Object(mut fields) = value else {
            return Err(invalid("geographic point"));
        };
        Ok(Self {
            lat: <f64 as DecodeValue<Number>>::decode_value(
                fields
                    .swap_remove("lat")
                    .ok_or_else(|| invalid("latitude"))?,
            )?,
            lng: <f64 as DecodeValue<Number>>::decode_value(
                fields
                    .swap_remove("lng")
                    .ok_or_else(|| invalid("longitude"))?,
            )?,
        })
    }
}

/// A read model can retain the protected view of a classified column.
#[derive(Debug, Clone, PartialEq)]
pub enum Protected<T> {
    Value(T),
    Masked {
        display: String,
        classification: String,
    },
}
impl<S, T: DecodeValue<S>> DecodeValue<S> for Protected<T> {
    fn decode_value(value: Value) -> Result<Self, DbError> {
        if value.get("sentinel").and_then(Value::as_str) == Some("__zsmask__")
            && value.get("_sig").and_then(Value::as_str)
                == Some(crate::protection::mask_pass::mask_sentinel_signature())
        {
            return Ok(Self::Masked {
                display: value
                    .get("masked")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("masked display"))?
                    .into(),
                classification: value
                    .get("classification")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("classification"))?
                    .into(),
            });
        }
        T::decode_value(value).map(Self::Value)
    }
}
