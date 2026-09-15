//! Serde adaptation at metadata and JSON-column boundaries.
use super::{Map, Number, Value};
use ::serde::{Serialize, de, ser};
use std::fmt;

#[derive(Debug)]
pub struct Error(String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}
impl ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

pub fn to_value<T: Serialize + ?Sized>(v: &T) -> Result<Value, Error> {
    v.serialize(Encoder)
}
pub fn from_value<T: de::DeserializeOwned>(v: Value) -> Result<T, Error> {
    T::deserialize(v)
}

pub struct Encoder;
impl ser::Serializer for Encoder {
    type Ok = Value;
    type Error = Error;
    type SerializeSeq = Sequence;
    type SerializeTuple = Sequence;
    type SerializeTupleStruct = Sequence;
    type SerializeTupleVariant = ser::Impossible<Value, Error>;
    type SerializeMap = Object;
    type SerializeStruct = Object;
    type SerializeStructVariant = ser::Impossible<Value, Error>;
    fn serialize_bool(self, v: bool) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_i8(self, v: i8) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_i16(self, v: i16) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_i32(self, v: i32) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_i64(self, v: i64) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_u8(self, v: u8) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_u16(self, v: u16) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_u32(self, v: u32) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_u64(self, v: u64) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_f32(self, v: f32) -> Result<Value, Error> {
        self.serialize_f64(f64::from(v))
    }
    fn serialize_f64(self, v: f64) -> Result<Value, Error> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| Error("non-finite database number".into()))
    }
    fn serialize_char(self, v: char) -> Result<Value, Error> {
        Ok(v.to_string().into())
    }
    fn serialize_str(self, v: &str) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Value, Error> {
        Ok(Value::Bytes(v.into()))
    }
    fn serialize_none(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<Value, Error> {
        to_value(v)
    }
    fn serialize_unit(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        v: &'static str,
    ) -> Result<Value, Error> {
        Ok(v.into())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> Result<Value, Error> {
        to_value(v)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        name: &'static str,
        v: &T,
    ) -> Result<Value, Error> {
        Ok(Value::Object([(name.into(), to_value(v)?)].into()))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Sequence, Error> {
        Ok(Sequence(Vec::with_capacity(len.unwrap_or(0))))
    }
    fn serialize_tuple(self, len: usize) -> Result<Sequence, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> Result<Sequence, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Error("tuple variants are not database values".into()))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Object, Error> {
        Ok(Object {
            fields: Map::new(),
            key: None,
        })
    }
    fn serialize_struct(self, _: &'static str, len: usize) -> Result<Object, Error> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Err(Error("struct variants are not database values".into()))
    }
}
pub struct Sequence(Vec<Value>);
impl ser::SerializeSeq for Sequence {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Error> {
        self.0.push(to_value(v)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Array(self.0))
    }
}
impl ser::SerializeTuple for Sequence {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Error> {
        ser::SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<Value, Error> {
        ser::SerializeSeq::end(self)
    }
}
impl ser::SerializeTupleStruct for Sequence {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Error> {
        ser::SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<Value, Error> {
        ser::SerializeSeq::end(self)
    }
}
pub struct Object {
    fields: Map<String, Value>,
    key: Option<String>,
}
impl ser::SerializeMap for Object {
    type Ok = Value;
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.key = Some(match to_value(key)? {
            Value::String(s) => s,
            _ => return Err(Error("record key must be text".into())),
        });
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| Error("missing record key".into()))?;
        self.fields.insert(key, to_value(v)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Object(self.fields))
    }
}
impl ser::SerializeStruct for Object {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<(), Error> {
        self.fields.insert(key.into(), to_value(v)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Object(self.fields))
    }
}
impl<'de> de::IntoDeserializer<'de, Error> for Value {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}
impl<'de> de::Deserializer<'de> for Value {
    type Error = Error;
    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Value::Null => visitor.visit_unit(),
            Value::Bool(v) => visitor.visit_bool(v),
            Value::Number(v) if v.is_i64() => visitor.visit_i64(v.as_i64().unwrap()),
            Value::Number(v) if v.is_u64() => visitor.visit_u64(v.as_u64().unwrap()),
            Value::Number(v) => {
                visitor.visit_f64(v.as_f64().ok_or_else(|| Error("invalid number".into()))?)
            }
            Value::String(v) | Value::Decimal(v) => visitor.visit_string(v),
            Value::TimestampMicros(v) => visitor.visit_i64(v),
            Value::Bytes(v) => visitor.visit_byte_buf(v),
            Value::Json(v) => serde_json::from_str::<Value>(&v)
                .map_err(|e| Error(e.to_string()))?
                .deserialize_any(visitor),
            Value::Array(v) => visitor.visit_seq(de::value::SeqDeserializer::new(v.into_iter())),
            Value::Object(v) => visitor.visit_map(de::value::MapDeserializer::new(v.into_iter())),
        }
    }
    fn deserialize_option<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }
    fn deserialize_newtype_struct<V: de::Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_seq<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if let Value::Bytes(bytes) = self {
            visitor.visit_seq(de::value::SeqDeserializer::new(bytes.into_iter()))
        } else {
            self.deserialize_any(visitor)
        }
    }
    fn deserialize_enum<V: de::Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Value::String(v) => visitor.visit_enum(de::value::StringDeserializer::<Error>::new(v)),
            _ => Err(Error("expected enum name".into())),
        }
    }
    ::serde::forward_to_deserialize_any! { bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf unit unit_struct tuple tuple_struct map struct identifier ignored_any }
}
