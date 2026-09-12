//! Validate native binary fields before they reach the SQL compiler.
use zeroship_data_orm::error::DbError;
use crate::value::Value;

pub fn schema_has_plain_bytes_columns(schema: &Value) -> bool {
    schema
        .as_object()
        .is_some_and(|fields| fields.values().any(is_plain_bytes))
}
fn is_plain_bytes(def: &Value) -> bool {
    !crate::sql::descriptors::is_encrypted(def) && def.get("type").and_then(Value::as_str) == Some("bytes")
}
pub fn validate_bytes_on_write(schema: &Value, doc: &mut Value) -> Result<(), DbError> {
    let Some(fields) = schema.as_object() else {
        return Ok(());
    };
    for (name, definition) in fields {
        if is_plain_bytes(definition) {
            if let Some(value) = doc.get(name) {
                validate_scalar(name, value)?;
            }
        }
    }
    Ok(())
}
pub fn validate_bytes_on_update(schema: &Value, patch: &mut Value) -> Result<(), DbError> {
    if let Some(set) = patch.get_mut("$set") {
        validate_bytes_on_write(schema, set)?;
    }
    let Some(fields) = schema.as_object() else {
        return Ok(());
    };
    for (name, definition) in fields {
        if is_plain_bytes(definition) {
            if let Some(value) = patch.get(name) {
                validate_scalar(name, value.get("$set").unwrap_or(value))?;
            }
        }
    }
    Ok(())
}
fn validate_scalar(field: &str, value: &Value) -> Result<(), DbError> {
    match value {
        Value::Null | Value::Bytes(_) => Ok(()),
        _ => Err(DbError::validation(
            "invalid_bytes_arg",
            format!("bytes column '{field}' requires native bytes"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;
    fn schema() -> Value {
        value!({ "payload": { "type": "bytes" }, "title": { "type": "string" } })
    }
    #[test]
    fn validation_preserves_the_owned_buffer_and_text() {
        let bytes = vec![0, 1, 255];
        let address = bytes.as_ptr();
        let mut doc = Value::Object(
            [
                ("payload".into(), Value::Bytes(bytes)),
                ("title".into(), "__zsbin_blob__:aGk=".into()),
            ]
            .into(),
        );
        validate_bytes_on_write(&schema(), &mut doc).unwrap();
        assert_eq!(doc["payload"].as_bytes().unwrap().as_ptr(), address);
        assert_eq!(doc["title"], "__zsbin_blob__:aGk=");
        assert_eq!(doc.as_object().unwrap().len(), 2);
    }
    #[test]
    fn absent_and_nullable_binary_fields_are_allowed() {
        for mut doc in [value!({}), value!({ "payload": null })] {
            validate_bytes_on_write(&schema(), &mut doc).unwrap();
        }
    }
    #[test]
    fn binary_fields_refuse_text_arrays_and_other_scalars() {
        for value in [
            value!("AAEC"),
            value!([0, 1, 2]),
            value!(true),
            value!(42),
            value!({}),
        ] {
            let mut doc = Value::Object([("payload".into(), value)].into());
            assert!(validate_bytes_on_write(&schema(), &mut doc).is_err());
        }
    }
    #[test]
    fn update_spellings_use_the_same_native_byte_contract() {
        for mut patch in [
            value!({ "payload": Value::Bytes(vec![1, 2]) }),
            value!({ "$set": { "payload": Value::Bytes(vec![1, 2]) } }),
            value!({ "payload": { "$set": Value::Bytes(vec![1, 2]) } }),
        ] {
            validate_bytes_on_update(&schema(), &mut patch).unwrap();
        }
        assert!(
            validate_bytes_on_update(&schema(), &mut value!({ "$set": { "payload": "AQI=" } }))
                .is_err()
        );
    }
    #[test]
    fn encrypted_fields_belong_to_the_encryption_pass() {
        let schema = value!({ "secret": { "type": "bytes", "encrypted": true } });
        assert!(!schema_has_plain_bytes_columns(&schema));
        validate_bytes_on_write(&schema, &mut value!({ "secret": Value::Bytes(vec![1]) })).unwrap();
    }
}
