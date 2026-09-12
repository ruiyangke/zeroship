//! Native plaintext encoding selected by the logical field type.

use crate::value::Value;
use base64::Engine as _;

use crate::{error::DbError, sql::statement::DecimalStorage};

#[derive(Clone, Copy, Debug)]
pub(crate) enum PlaintextType {
    String,
    Number,
    ExactDecimal(DecimalStorage),
    Bytes,
}

impl PlaintextType {
    pub(crate) fn from_field(field: &Value) -> Result<Option<Self>, DbError> {
        let Some(flag) = field.get("encrypted") else {
            return Ok(None);
        };
        let encrypted = flag.as_bool().ok_or_else(|| {
            DbError::validation("invalid_encryption_metadata", "encrypted must be a boolean")
        })?;
        if !encrypted {
            return Ok(None);
        }
        match field.get("type").and_then(Value::as_str) {
            Some("string") => Ok(Some(Self::String)),
            Some("number") if field.get("precision").is_some() => {
                let storage = crate::sql::decimal::storage(field).map_err(|error| {
                    DbError::validation("invalid_numeric_metadata", error.to_string())
                })?;
                Ok(Some(Self::ExactDecimal(storage.ok_or_else(|| {
                    DbError::validation(
                        "invalid_numeric_metadata",
                        "fixed precision number requires decimal storage",
                    )
                })?)))
            }
            Some("number") => Ok(Some(Self::Number)),
            Some("bytes") => Ok(Some(Self::Bytes)),
            _ => Err(DbError::validation(
                "encrypted_type_unsupported",
                "encrypted field type must be string, number, or bytes",
            )),
        }
    }

    pub(crate) fn encode(self, value: &Value) -> Result<Vec<u8>, DbError> {
        let bytes = match self {
            Self::String => value.as_str().map(|s| s.as_bytes().to_vec()),
            Self::Number => value
                .as_f64()
                .filter(|n| n.is_finite())
                .map(|n| n.to_be_bytes().to_vec()),
            Self::ExactDecimal(storage) => match value {
                Value::Decimal(value) if crate::sql::decimal::valid(value) => {
                    crate::sql::decimal::quantize(value, storage)
                        .ok()
                        .map(String::into_bytes)
                }
                _ => None,
            },
            Self::Bytes => value.as_bytes().map(<[u8]>::to_vec),
        };
        bytes.ok_or_else(|| {
            DbError::validation(
                "encrypted_value_type_mismatch",
                format!("encrypted value must have type {self:?}"),
            )
        })
    }

    pub(crate) fn decode(self, bytes: &[u8]) -> Result<Value, DbError> {
        match self {
            Self::String => Ok(Value::String(
                std::str::from_utf8(bytes)
                    .map_err(|_| DbError::internal("decrypted plaintext is not valid UTF-8"))?
                    .to_owned(),
            )),
            Self::Number => {
                let bytes = bytes.try_into().map_err(|_| {
                    DbError::internal("decrypted number has an invalid encoded length")
                })?;
                Value::try_from(f64::from_be_bytes(bytes)).map_err(DbError::internal)
            }
            Self::ExactDecimal(storage) => {
                let value = std::str::from_utf8(bytes)
                    .map_err(|_| DbError::internal("decrypted decimal is not valid UTF-8"))?;
                crate::sql::decimal::quantize(value, storage)
                    .map(Value::Decimal)
                    .map_err(|_| DbError::internal("decrypted decimal is not valid"))
            }
            Self::Bytes => Ok(Value::Bytes(bytes.to_vec())),
        }
    }

    /// Render already encoded plaintext for mask derivation.
    pub(crate) fn mask_text(self, bytes: &[u8]) -> String {
        match self {
            Self::String => std::str::from_utf8(bytes)
                .expect("encoded string")
                .to_owned(),
            Self::Number => {
                f64::from_be_bytes(bytes.try_into().expect("encoded number")).to_string()
            }
            Self::ExactDecimal(_) => std::str::from_utf8(bytes)
                .expect("encoded decimal")
                .to_owned(),
            Self::Bytes => base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn encrypted_plaintext_round_trips_native_values_from_field_types() {
        for (name, value) in [
            ("string", Value::from("hello")),
            ("number", Value::try_from(42.25).unwrap()),
            ("bytes", Value::Bytes(vec![0, 0xff, 0x80])),
        ] {
            let codec = PlaintextType::from_field(&value!({"type": name, "encrypted": true}))
                .unwrap()
                .unwrap();
            let bytes = codec.encode(&value).unwrap();
            assert_eq!(codec.decode(&bytes).unwrap(), value);
        }
    }

    #[test]
    fn encrypted_exact_decimal_round_trips_without_floating_point() {
        let codec = PlaintextType::from_field(&value!({
            "type": "number",
            "precision": 30,
            "scale": 2,
            "encrypted": true
        }))
        .unwrap()
        .unwrap();
        let value = Value::Decimal("9007199254740993.005".into());
        let bytes = codec.encode(&value).unwrap();
        assert_eq!(
            codec.decode(&bytes).unwrap(),
            Value::Decimal("9007199254740993.01".into())
        );
        assert_eq!(codec.mask_text(&bytes), "9007199254740993.01");
    }

    #[test]
    fn encrypted_plaintext_rejects_invalid_encoding() {
        assert!(PlaintextType::String.decode(&[0xff]).is_err());
        assert!(PlaintextType::Number.decode(&[0]).is_err());
        for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(PlaintextType::Number.decode(&number.to_be_bytes()).is_err());
        }
        assert!(PlaintextType::String.encode(&Value::from(42)).is_err());
    }

    #[test]
    fn plaintext_fields_do_not_select_an_encryption_codec() {
        for field in [
            value!({"type":"bytes"}),
            value!({"type":"bytes", "encrypted":false}),
        ] {
            assert!(PlaintextType::from_field(&field).unwrap().is_none());
        }
        assert!(PlaintextType::from_field(&value!({"type":"boolean", "encrypted":true})).is_err());
    }
}
