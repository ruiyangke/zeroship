//! Descriptor-directed temporal values and typed JSON containers.
use super::{CodecError, Value};

const MAX_TYPED_DEPTH: usize = 128;

fn invalid(field: &str, expected: &str) -> CodecError {
    CodecError::validation(
        "invalid_typed_value",
        format!("column '{field}' requires {expected}"),
    )
}

fn scalar(kind: &str, field: &str, value: &mut Value) -> Result<(), CodecError> {
    if kind == "calendarDate" {
        if value
            .as_str()
            .is_none_or(|date| crate::sql::temporal::parse_calendar_date(date).is_none())
        {
            return Err(CodecError::validation(
                "invalid_calendar_date",
                format!("column '{field}' requires a valid YYYY-MM-DD calendar date"),
            ));
        }
    } else {
        let millis = crate::sql::temporal::timestamp_millis(value).ok_or_else(|| {
            CodecError::validation(
                "invalid_timestamp",
                format!(
                    "column '{field}' requires a portable timestamp or integral Unix milliseconds"
                ),
            )
        })?;
        *value = Value::from(millis);
    }
    Ok(())
}

fn temporal_item(definition: &Value) -> Option<&str> {
    (definition["type"].as_str() == Some("array"))
        .then(|| definition["items"].as_str())
        .flatten()
        .filter(|kind| matches!(*kind, "date" | "timestamp" | "calendarDate"))
}

fn array_item<'a>(field: &str, definition: &'a Value) -> Result<&'a str, CodecError> {
    let item = match definition.get("items") {
        None => "json",
        Some(item) => item.as_str().unwrap_or(""),
    };
    if matches!(
        item,
        "string" | "number" | "boolean" | "date" | "timestamp" | "calendarDate" | "json"
    ) {
        Ok(item)
    } else {
        Err(CodecError::validation(
            "invalid_array_item_type",
            format!("column '{field}' must declare a supported array item type"),
        ))
    }
}

fn invalid_element(field: &str, item: &str) -> CodecError {
    CodecError::validation(
        "invalid_array_element",
        format!("column '{field}' requires {item} array elements"),
    )
}

fn raw_element_matches(item: &str, value: &serde_json::value::RawValue) -> bool {
    match item {
        "string" => value.get().starts_with('"'),
        "number" => value
            .get()
            .starts_with(|c: char| c == '-' || c.is_ascii_digit()),
        "boolean" => matches!(value.get(), "true" | "false"),
        "json" => true,
        _ => false,
    }
}

fn validate_encoded_array(field: &str, item: &str, json: &str) -> Result<(), CodecError> {
    let raw: &serde_json::value::RawValue =
        serde_json::from_str(json).map_err(|_| invalid(field, "valid typed JSON"))?;
    if raw.get() == "null" {
        return Ok(());
    }
    let elements: Vec<&serde_json::value::RawValue> =
        serde_json::from_str(raw.get()).map_err(|_| invalid(field, "an array"))?;
    for value in elements {
        if !raw_element_matches(item, value) {
            return Err(invalid_element(field, item));
        }
    }
    Ok(())
}

fn prepare_array_element(field: &str, item: &str, value: &mut Value) -> Result<(), CodecError> {
    if matches!(item, "date" | "timestamp" | "calendarDate") {
        return scalar(item, field, value);
    }
    let valid = match value {
        Value::Json(json) => serde_json::from_str::<&serde_json::value::RawValue>(json)
            .is_ok_and(|raw| raw_element_matches(item, raw)),
        _ => match item {
            "string" => value.is_string(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "json" => true,
            _ => false,
        },
    };
    if !valid {
        return Err(invalid_element(field, item));
    }
    Ok(())
}

/// Prepare a field before storage or parameter binding. Untyped JSON is opaque.
///
/// # Errors
/// Refuses invalid typed values, containers, or excessive nesting.
pub fn prepare_value(field: &str, definition: &Value, value: &mut Value) -> Result<(), CodecError> {
    prepare_value_at(field, definition, value, 0)
}

fn prepare_value_at(
    field: &str,
    definition: &Value,
    value: &mut Value,
    depth: usize,
) -> Result<(), CodecError> {
    if depth > MAX_TYPED_DEPTH {
        return Err(invalid(field, "bounded typed nesting"));
    }
    if value.is_null() {
        return Ok(());
    }
    let kind = definition["type"].as_str();
    if matches!(kind, Some("date" | "timestamp" | "calendarDate")) {
        return scalar(kind.unwrap(), field, value);
    }
    if !matches!(kind, Some("array" | "object" | "union")) {
        return Ok(());
    }
    if kind == Some("array") {
        let item = array_item(field, definition)?;
        if let Value::Json(json) = value {
            if temporal_item(definition).is_none() {
                return validate_encoded_array(field, item, json);
            }
        }
    }
    if let Value::Json(json) = value {
        let parsed: Value =
            serde_json::from_str(json).map_err(|_| invalid(field, "valid typed JSON"))?;
        if parsed.is_null() {
            return Ok(());
        }
        *value = parsed;
    }
    if kind == Some("array") {
        let item = array_item(field, definition)?;
        let values = value
            .as_array_mut()
            .ok_or_else(|| invalid(field, "an array"))?;
        for value in values {
            prepare_array_element(field, item, value)?;
        }
        return Ok(());
    }
    let shape = if kind == Some("union") {
        let discriminator = definition["discriminator"]
            .as_str()
            .ok_or_else(|| invalid(field, "a union discriminator"))?;
        definition["variants"]
            .as_array()
            .and_then(|variants| {
                variants.iter().find(|variant| {
                    value.get(discriminator).is_some_and(|actual| {
                        variant
                            .get(discriminator)
                            .and_then(|def| def.get("literalValue"))
                            == Some(actual)
                    })
                })
            })
            .ok_or_else(|| invalid(field, "a declared union variant"))?
    } else {
        &definition["shape"]
    };
    prepare_document_at(shape, value, field, depth + 1)
}

/// Validate and normalize writes before protection changes their storage shape.
///
/// # Errors
/// Refuses invalid typed values, containers, or excessive nesting.
pub fn prepare_document(schema: &Value, document: &mut Value) -> Result<(), CodecError> {
    if !document.is_object() {
        return Ok(());
    }
    prepare_document_at(schema, document, "", 0)
}

fn prepare_document_at(
    schema: &Value,
    document: &mut Value,
    prefix: &str,
    depth: usize,
) -> Result<(), CodecError> {
    let Some(document) = document.as_object_mut() else {
        return Err(invalid(prefix, "an object"));
    };
    for (field, value) in document {
        if let Some(definition) = schema.get(field) {
            if !matches!(
                definition["type"].as_str(),
                Some("date" | "timestamp" | "calendarDate" | "array" | "object" | "union")
            ) {
                continue;
            }
            let path = if prefix.is_empty() {
                field.clone()
            } else {
                format!("{prefix}.{field}")
            };
            prepare_value_at(&path, definition, value, depth)?;
        }
    }
    Ok(())
}

/// Normalize an array-operation operand using the declared primitive item type.
///
/// # Errors
/// Refuses an operand that does not satisfy the declared item type.
pub fn prepare_array_operand(
    field: &str,
    definition: &Value,
    value: &mut Value,
) -> Result<(), CodecError> {
    if definition["type"].as_str() != Some("array") {
        return Ok(());
    }
    prepare_array_element(field, array_item(field, definition)?, value)
}

/// Normalize typed assignments and array operations before protection.
///
/// # Errors
/// Refuses invalid typed operands and unsupported temporal operations.
pub fn prepare_update(schema: &Value, patch: &mut Value) -> Result<(), CodecError> {
    let Some(patch) = patch.as_object_mut() else {
        return Ok(());
    };
    for (field, value) in patch {
        if field == "$set" {
            prepare_document(schema, value)?;
            continue;
        }
        let Some(definition) = schema.get(field) else {
            continue;
        };
        if let Some(operations) = value
            .as_object_mut()
            .filter(|ops| ops.keys().any(|key| key.starts_with('$')))
        {
            for (operation, operand) in operations {
                use crate::sql::update::Operator;
                let operation = Operator::parse(operation)?;
                operation.validate_type(field, definition)?;
                match operation {
                    Operator::Set => prepare_value(field, definition, operand)?,
                    Operator::Push | Operator::Pull | Operator::AddToSet => {
                        prepare_array_operand(field, definition, operand)?;
                    }
                    Operator::Increment | Operator::Decrement | Operator::Multiply => {}
                }
            }
        } else {
            prepare_value(field, definition, value)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn array_validation_preserves_native_buffers_and_encoded_numbers() {
        let definition = value!({"type":"array","items":"string"});
        let text = String::from("native input");
        let address = text.as_ptr();
        let mut data = Value::Array(vec![Value::String(text)]);
        prepare_value("names", &definition, &mut data).unwrap();
        assert_eq!(data[0].as_str().unwrap().as_ptr(), address);

        let json = "[1.00000000000000000000000001,18446744073709551616]";
        let mut data = Value::Json(json.into());
        prepare_value(
            "amounts",
            &value!({"type":"array","items":"number"}),
            &mut data,
        )
        .unwrap();
        assert_eq!(data, Value::Json(json.into()));
    }

    #[test]
    fn arrays_validate_encoded_items_and_refuse_unknown_item_types() {
        for (item, json) in [
            ("string", "[false]"),
            ("number", "[true]"),
            ("boolean", "[1]"),
            ("string", "[null]"),
        ] {
            let error = prepare_value(
                "values",
                &value!({"type":"array","items":item}),
                &mut Value::Json(json.into()),
            )
            .unwrap_err();
            assert!(matches!(
                error,
                CodecError::Validation {
                    code: "invalid_array_element",
                    ..
                }
            ));
        }
        for item in [value!("unknown"), value!(42), value!(null)] {
            assert!(matches!(
                prepare_value(
                    "values",
                    &value!({"type":"array","items":item}),
                    &mut value!([])
                ),
                Err(CodecError::Validation {
                    code: "invalid_array_item_type",
                    ..
                })
            ));
        }
        for mut data in [
            value!([1, "text", null, [true]]),
            Value::Json("[1,\"text\",null,[true]]".into()),
        ] {
            prepare_value("values", &value!({"type":"array"}), &mut data).unwrap();
        }
    }

    #[test]
    fn temporal_arrays_reject_arithmetic_and_invalid_item_operations() {
        let schema = value!({"instants":{"type":"array","items":"timestamp"}});
        for mut patch in [
            value!({"instants":{"$inc":1}}),
            value!({"instants":{"$push":null}}),
            value!({"instants":{"$push":[0]}}),
            value!({"instants":{"$addToSet":"private_not_a_timestamp"}}),
            value!({"instants":{"$pull":"2026-02-30"}}),
        ] {
            let error = prepare_update(&schema, &mut patch).unwrap_err();
            assert!(!error.to_string().contains("private_not_a_timestamp"));
        }
    }

    #[test]
    fn temporal_nesting_is_bounded_and_json_null_remains_json() {
        let mut schema = value!({"type":"timestamp"});
        let mut data = value!(0);
        for _ in 0..=MAX_TYPED_DEPTH {
            schema = value!({"type":"object","shape":{"child":schema}});
            data = value!({"child":data});
        }
        assert!(prepare_value("nested", &schema, &mut data).is_err());

        let mut data = Value::Json("null".into());
        prepare_value(
            "instants",
            &value!({"type":"array","items":"date"}),
            &mut data,
        )
        .unwrap();
        assert_eq!(data, Value::Json("null".into()));
    }
}
