//! Native values shared by query compilation, protection passes and adapters.
//!
//! Bytes remain bytes until a transport encodes them. JSON serialization is an
//! explicit boundary operation, used for JSON columns and persisted metadata.

use ::serde::{Deserialize, Deserializer, Serialize, Serializer};
use indexmap::IndexMap;
use std::fmt;
use std::ops::{Index, IndexMut};

pub use serde_json::Number;
pub type Map<K, V> = IndexMap<K, V>;
pub type Record = Map<String, Value>;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Bytes(Vec<u8>),
    /// Unix milliseconds. The descriptor selects the database timestamp type.
    Timestamp(i64),
    /// Exact decimal spelling; never routed through a floating point value.
    Decimal(String),
    /// Encoded JSON storage value. This tag distinguishes JSON strings from SQL text.
    Json(String),
    Array(Vec<Value>),
    Object(Record),
}

impl Value {
    pub fn as_object(&self) -> Option<&Record> {
        if let Self::Object(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_object_mut(&mut self) -> Option<&mut Record> {
        if let Self::Object(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Self>> {
        if let Self::Array(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Self>> {
        if let Self::Array(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(v) => v.as_i64(),
            Self::Timestamp(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        if let Self::Number(v) = self {
            v.as_u64()
        } else {
            None
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        if let Self::Number(v) = self {
            v.as_f64()
        } else {
            None
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if let Self::Bytes(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
    pub fn is_boolean(&self) -> bool {
        matches!(self, Self::Bool(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self, Self::Number(_))
    }
    pub fn is_i64(&self) -> bool {
        self.as_i64().is_some()
    }
    pub fn is_u64(&self) -> bool {
        self.as_u64().is_some()
    }
    pub fn is_f64(&self) -> bool {
        matches!(self, Self::Number(v) if v.is_f64())
    }
    pub fn is_string(&self) -> bool {
        matches!(self, Self::String(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
    }
    pub fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
    pub fn get<I: ValueIndex>(&self, index: I) -> Option<&Self> {
        index.get(self)
    }
    pub fn get_mut<I: ValueIndex>(&mut self, index: I) -> Option<&mut Self> {
        index.get_mut(self)
    }
}

pub trait ValueIndex {
    fn get(self, value: &Value) -> Option<&Value>;
    fn get_mut(self, value: &mut Value) -> Option<&mut Value>;
    fn insert(self, value: &mut Value) -> &mut Value;
}
impl ValueIndex for &str {
    fn get(self, value: &Value) -> Option<&Value> {
        value.as_object()?.get(self)
    }
    fn get_mut(self, value: &mut Value) -> Option<&mut Value> {
        value.as_object_mut()?.get_mut(self)
    }
    fn insert(self, value: &mut Value) -> &mut Value {
        if value.is_null() {
            *value = Value::Object(Record::new());
        }
        value
            .as_object_mut()
            .expect("index requires a record")
            .entry(self.to_owned())
            .or_default()
    }
}
impl ValueIndex for &String {
    fn get(self, value: &Value) -> Option<&Value> {
        ValueIndex::get(self.as_str(), value)
    }
    fn get_mut(self, value: &mut Value) -> Option<&mut Value> {
        ValueIndex::get_mut(self.as_str(), value)
    }
    fn insert(self, value: &mut Value) -> &mut Value {
        self.as_str().insert(value)
    }
}
impl ValueIndex for usize {
    fn get(self, value: &Value) -> Option<&Value> {
        value.as_array()?.get(self)
    }
    fn get_mut(self, value: &mut Value) -> Option<&mut Value> {
        value.as_array_mut()?.get_mut(self)
    }
    fn insert(self, value: &mut Value) -> &mut Value {
        &mut value.as_array_mut().expect("index requires an array")[self]
    }
}
impl<I: ValueIndex> Index<I> for Value {
    type Output = Value;
    fn index(&self, index: I) -> &Value {
        index.get(self).unwrap_or(&Value::Null)
    }
}
impl<I: ValueIndex> IndexMut<I> for Value {
    fn index_mut(&mut self, index: I) -> &mut Value {
        index.insert(self)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::String(v.into())
    }
}
impl From<&String> for Value {
    fn from(v: &String) -> Self {
        Self::String(v.clone())
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<Number> for Value {
    fn from(v: Number) -> Self {
        Self::Number(v)
    }
}
impl From<Record> for Value {
    fn from(v: Record) -> Self {
        Self::Object(v)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Self::Array(v)
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map(Into::into).unwrap_or(Self::Null)
    }
}
impl From<&Value> for Value {
    fn from(v: &Value) -> Self {
        v.clone()
    }
}
macro_rules! integers {
    ($($t:ty),*) => { $(impl From<$t> for Value { fn from(v: $t) -> Self { Self::Number(v.into()) } }
        impl PartialEq<$t> for Value { fn eq(&self, v: &$t) -> bool { self == &Value::from(*v) } })* };
}
integers!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
impl PartialEq<str> for Value {
    fn eq(&self, v: &str) -> bool {
        self.as_str() == Some(v)
    }
}
impl PartialEq<&str> for Value {
    fn eq(&self, v: &&str) -> bool {
        self.as_str() == Some(*v)
    }
}
impl PartialEq<String> for Value {
    fn eq(&self, v: &String) -> bool {
        self.as_str() == Some(v)
    }
}
impl PartialEq<bool> for Value {
    fn eq(&self, v: &bool) -> bool {
        self.as_bool() == Some(*v)
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Bool(v) => serializer.serialize_bool(*v),
            Self::Number(v) => v.serialize(serializer),
            Self::String(v) | Self::Decimal(v) => serializer.serialize_str(v),
            Self::Bytes(v) => serializer.serialize_bytes(v),
            Self::Timestamp(v) => serializer.serialize_i64(*v),
            Self::Json(v) => {
                let json: Value = serde_json::from_str(v).map_err(::serde::ser::Error::custom)?;
                json.serialize(serializer)
            }
            Self::Array(v) => v.serialize(serializer),
            Self::Object(v) => v.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> ::serde::de::Visitor<'de> for Visitor {
            type Value = Value;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a database value")
            }
            fn visit_unit<E: ::serde::de::Error>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_none<E: ::serde::de::Error>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_bool<E: ::serde::de::Error>(self, v: bool) -> Result<Value, E> {
                Ok(v.into())
            }
            fn visit_i64<E: ::serde::de::Error>(self, v: i64) -> Result<Value, E> {
                Ok(v.into())
            }
            fn visit_u64<E: ::serde::de::Error>(self, v: u64) -> Result<Value, E> {
                Ok(v.into())
            }
            fn visit_f64<E: ::serde::de::Error>(self, v: f64) -> Result<Value, E> {
                Number::from_f64(v)
                    .map(Value::Number)
                    .ok_or_else(|| E::custom("non-finite database number"))
            }
            fn visit_str<E: ::serde::de::Error>(self, v: &str) -> Result<Value, E> {
                Ok(v.into())
            }
            fn visit_string<E: ::serde::de::Error>(self, v: String) -> Result<Value, E> {
                Ok(v.into())
            }
            fn visit_bytes<E: ::serde::de::Error>(self, v: &[u8]) -> Result<Value, E> {
                Ok(Value::Bytes(v.into()))
            }
            fn visit_byte_buf<E: ::serde::de::Error>(self, v: Vec<u8>) -> Result<Value, E> {
                Ok(Value::Bytes(v))
            }
            fn visit_seq<A: ::serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    values.push(value);
                }
                Ok(Value::Array(values))
            }
            fn visit_map<A: ::serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Value, A::Error> {
                let mut values = Record::new();
                while let Some((key, value)) = map.next_entry()? {
                    values.insert(key, value);
                }
                Ok(Value::Object(values))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display is a serialization boundary, never the SQL binding path.
        serde_json::to_string(self).map_err(|_| fmt::Error)?.fmt(f)
    }
}

/// Decode JSON metadata or a JSON column at its boundary.
impl From<serde_json::Value> for Value {
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(v) => Self::Bool(v),
            serde_json::Value::Number(v) => Self::Number(v),
            serde_json::Value::String(v) => Self::String(v),
            serde_json::Value::Array(v) => Self::Array(v.into_iter().map(Into::into).collect()),
            serde_json::Value::Object(v) => {
                Self::Object(v.into_iter().map(|(k, v)| (k, v.into())).collect())
            }
        }
    }
}

mod macros;
mod serde;
pub use self::serde::{from_value, to_value};
pub use crate::value;

impl From<char> for Value {
    fn from(v: char) -> Self {
        Self::String(v.to_string())
    }
}

impl ValueIndex for String {
    fn get(self, value: &Value) -> Option<&Value> {
        ValueIndex::get(self.as_str(), value)
    }
    fn get_mut(self, value: &mut Value) -> Option<&mut Value> {
        ValueIndex::get_mut(self.as_str(), value)
    }
    fn insert(self, value: &mut Value) -> &mut Value {
        if value.is_null() {
            *value = Value::Object(Record::new());
        }
        value
            .as_object_mut()
            .expect("index requires record")
            .entry(self)
            .or_default()
    }
}

impl TryFrom<f64> for Value {
    type Error = &'static str;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Number::from_f64(value)
            .map(Self::Number)
            .ok_or("non-finite database number")
    }
}
impl TryFrom<f32> for Value {
    type Error = &'static str;
    fn try_from(value: f32) -> Result<Self, Self::Error> {
        Self::try_from(f64::from(value))
    }
}
