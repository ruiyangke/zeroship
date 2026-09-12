//! Complete row keys shared by protection, mutation probes and unmask reads.

use crate::{
    error::DbError,
    sql::descriptors::primary_key_fields,
    value::{Record, Value},
};
use base64::Engine;

fn invalid() -> DbError {
    DbError::validation(
        "invalid_row_identity",
        "row identity must contain the complete declared key",
    )
}

pub(crate) fn key(schema: &Value, row: &Record) -> Result<Record, DbError> {
    primary_key_fields(schema)
        .map_err(|_| invalid())?
        .into_iter()
        .map(|name| {
            row.get(name)
                .filter(|value| !value.is_null())
                .cloned()
                .map(|value| (name.to_owned(), value))
                .ok_or_else(invalid)
        })
        .collect()
}

/// A scalar key remains a scalar token; compound keys carry their field names.
/// The token is opaque to the mask adapter and never substitutes for a SQL filter.
pub(crate) fn token(schema: &Value, row: &Record) -> Option<String> {
    let mut key = key(schema, row).ok()?;
    if key.len() == 1 {
        return match key.values().next()? {
            Value::String(value) | Value::Decimal(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            Value::Timestamp(value) => Some(value.to_string()),
            Value::Bool(value) => Some(value.to_string()),
            Value::Bytes(value) => Some(base64::engine::general_purpose::STANDARD.encode(value)),
            _ => None,
        };
    }
    key.sort_keys();
    serde_json::to_string(&Value::Object(key)).ok()
}

pub(crate) fn from_token(schema: &Value, token: &str) -> Result<Record, DbError> {
    let fields = primary_key_fields(schema).map_err(|_| invalid())?;
    let mut values = if fields.len() == 1 {
        let name = fields[0];
        let value = match schema[name]["type"].as_str() {
            Some("int" | "integer" | "bigInt") => {
                Value::from(token.parse::<i64>().map_err(|_| invalid())?)
            }
            Some("number" | "float") => {
                Value::Number(token.parse::<serde_json::Number>().map_err(|_| invalid())?)
            }
            Some("boolean") => Value::Bool(token.parse().map_err(|_| invalid())?),
            Some("date" | "timestamp") => Value::Timestamp(token.parse().map_err(|_| invalid())?),
            Some("decimal" | "numeric") => Value::Decimal(token.into()),
            Some("bytes") => Value::Bytes(
                base64::engine::general_purpose::STANDARD
                    .decode(token)
                    .map_err(|_| invalid())?,
            ),
            _ => Value::from(token),
        };
        [(name.to_owned(), value)].into()
    } else {
        let Value::Object(values) = serde_json::from_str(token).map_err(|_| invalid())? else {
            return Err(invalid());
        };
        values
    };
    if values.len() != fields.len() {
        return Err(invalid());
    }
    for name in fields {
        let value = values
            .get_mut(name)
            .filter(|value| !value.is_null())
            .ok_or_else(invalid)?;
        crate::sql::codecs::prepare_value(name, &schema[name], value).map_err(|_| invalid())?;
        match (schema[name]["type"].as_str(), &*value) {
            (Some("bytes"), Value::Array(bytes)) => {
                let bytes = bytes
                    .iter()
                    .map(|byte| {
                        byte.as_u64()
                            .and_then(|byte| u8::try_from(byte).ok())
                            .ok_or_else(invalid)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                *value = Value::Bytes(bytes);
            }
            (Some("decimal" | "numeric"), Value::String(decimal)) => {
                *value = Value::Decimal(decimal.clone());
            }
            (Some("date" | "timestamp"), _) => {
                *value = Value::Timestamp(
                    crate::sql::temporal::timestamp_millis(value).ok_or_else(invalid)?,
                );
            }
            _ => {}
        }
    }
    Ok(values)
}

/// Named and compound keys have a binary domain distinct from scalar `id`
/// bytes. A creator-selected string cannot impersonate an encoded compound key.
pub(crate) fn aad<'a>(
    schema: &Value,
    identity: &'a str,
) -> Result<std::borrow::Cow<'a, [u8]>, DbError> {
    let mut fields = primary_key_fields(schema).map_err(|_| invalid())?;
    if fields == ["id"] {
        return Ok(identity.as_bytes().into());
    }
    let canonical = if fields.len() > 1 {
        token(schema, &from_token(schema, identity)?).ok_or_else(invalid)?
    } else {
        identity.to_owned()
    };
    fields.sort_unstable();
    let mut bytes = vec![0xff];
    bytes.extend(serde_json::to_vec(&(fields, canonical)).map_err(|_| invalid())?);
    Ok(bytes.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn compound_identity_round_trips_without_delimiter_aliases() {
        let schema = value!({"app":{"type":"string","primaryKey":true},"run":{"type":"string","primaryKey":true},"generation":{"type":"bigInt","primaryKey":true}});
        let first = value!({"app":"a:b", "run":"c", "generation":7});
        let second = value!({"app":"a", "run":"b:c", "generation":7});
        let first_token = token(&schema, first.as_object().unwrap()).unwrap();
        assert_ne!(
            first_token,
            token(&schema, second.as_object().unwrap()).unwrap()
        );
        assert_eq!(
            from_token(&schema, &first_token).unwrap(),
            *first.as_object().unwrap()
        );
        assert!(from_token(&schema, r#"{"app":"a","run":"c"}"#).is_err());
        assert!(from_token(&schema, r#"{"app":"a","run":"c","generation":null}"#).is_err());
        assert!(
            from_token(
                &schema,
                r#"{"app":"a","run":"c","generation":7,"extra":true}"#
            )
            .is_err()
        );
        let scalar = value!({"id":{"type":"string","primaryKey":true}});
        assert_ne!(
            aad(&schema, &first_token).unwrap(),
            aad(&scalar, &first_token).unwrap()
        );
        assert_eq!(
            aad(&schema, &first_token).unwrap(),
            aad(&schema, r#"{"generation":7,"run":"c","app":"a:b"}"#).unwrap()
        );
    }

    #[test]
    fn native_key_values_survive_the_opaque_token_boundary() {
        let schema = value!({
            "bytes":{"type":"bytes","primaryKey":true},
            "instant":{"type":"timestamp","primaryKey":true},
            "decimal":{"type":"decimal","primaryKey":true}
        });
        let row: Record = [
            ("bytes".into(), Value::Bytes(vec![0, 255, 1])),
            ("instant".into(), Value::Timestamp(-1)),
            ("decimal".into(), Value::Decimal("1.25".into())),
        ]
        .into();
        let encoded = token(&schema, &row).unwrap();
        assert_eq!(from_token(&schema, &encoded).unwrap(), row);
        let schema = value!({"key":{"type":"bytes","primaryKey":true}});
        let row: Record = [("key".into(), Value::Bytes(vec![0, 255, 1]))].into();
        assert_eq!(
            from_token(&schema, &token(&schema, &row).unwrap()).unwrap(),
            row
        );
    }
}
