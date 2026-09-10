//! Descriptor-directed temporal values, including typed JSON containers.
use super::{CodecError, Value};

const MAX_TEMPORAL_DEPTH: usize = 128;

fn invalid(field: &str, expected: &str) -> CodecError {
    CodecError::validation(
        "invalid_temporal_value",
        format!("column '{field}' requires {expected}"),
    )
}

fn scalar(kind: &str, field: &str, value: &mut Value) -> Result<(), CodecError> {
    if kind == "calendarDate" {
        if value
            .as_str()
            .is_none_or(|date| crate::temporal::parse_calendar_date(date).is_none())
        {
            return Err(CodecError::validation(
                "invalid_calendar_date",
                format!("column '{field}' requires a valid YYYY-MM-DD calendar date"),
            ));
        }
    } else {
        let millis = crate::temporal::timestamp_millis(value).ok_or_else(|| {
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

/// Prepare a field before storage or parameter binding. Untyped JSON is opaque.
///
/// # Errors
/// Refuses invalid temporal values, typed containers, or excessive nesting.
pub fn prepare_value(
    field: &str,
    definition: &Value,
    value: &mut Value,
) -> Result<(), CodecError> {
    prepare_value_at(field, definition, value, 0)
}

fn prepare_value_at(
    field: &str,
    definition: &Value,
    value: &mut Value,
    depth: usize,
) -> Result<(), CodecError> {
    if depth > MAX_TEMPORAL_DEPTH {
        return Err(invalid(field, "bounded temporal nesting"));
    }
    if value.is_null() {
        return Ok(());
    }
    let kind = definition["type"].as_str();
    if matches!(kind, Some("date" | "timestamp" | "calendarDate")) {
        return scalar(kind.unwrap(), field, value);
    }
    if !matches!(kind, Some("object" | "union")) && temporal_item(definition).is_none() {
        return Ok(());
    }
    if let Value::Json(json) = value {
        let parsed: Value =
            serde_json::from_str(json).map_err(|_| invalid(field, "valid typed JSON"))?;
        if parsed.is_null() {
            return Ok(());
        }
        *value = parsed;
    }
    if let Some(item) = temporal_item(definition) {
        let values = value
            .as_array_mut()
            .ok_or_else(|| invalid(field, "an array"))?;
        for (index, value) in values.iter_mut().enumerate() {
            scalar(item, &format!("{field}[{index}]"), value)?;
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
/// Refuses invalid temporal values, typed containers, or excessive nesting.
pub fn prepare_temporal_document(schema: &Value, document: &mut Value) -> Result<(), CodecError> {
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
                Some("date" | "timestamp" | "calendarDate" | "object" | "union")
            ) && temporal_item(definition).is_none()
            {
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
/// Refuses an operand that does not satisfy the temporal item type.
pub fn prepare_array_operand(
    field: &str,
    definition: &Value,
    operation: &str,
    value: &mut Value,
) -> Result<(), CodecError> {
    let Some(item) = temporal_item(definition) else {
        return Ok(());
    };
    if operation != "$pull" {
        if let Some(values) = value.as_array_mut() {
            for (index, value) in values.iter_mut().enumerate() {
                scalar(item, &format!("{field}[{index}]"), value)?;
            }
            return Ok(());
        }
    }
    scalar(item, field, value)
}

/// Normalize assignments and temporal array operations before protection.
///
/// # Errors
/// Refuses invalid temporal operands and unsupported temporal operations.
pub fn prepare_temporal_update(schema: &Value, patch: &mut Value) -> Result<(), CodecError> {
    let Some(patch) = patch.as_object_mut() else {
        return Ok(());
    };
    for (field, value) in patch {
        if field == "$set" {
            prepare_temporal_document(schema, value)?;
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
                match operation.as_str() {
                    "$set" => prepare_value(field, definition, operand)?,
                    "$push" | "$pull" | "$addToSet" if temporal_item(definition).is_some() => {
                        prepare_array_operand(field, definition, operation, operand)?;
                    }
                    _ if matches!(
                        definition["type"].as_str(),
                        Some("calendarDate" | "date" | "timestamp")
                    ) || temporal_item(definition).is_some() =>
                    {
                        let code = match definition["type"].as_str() {
                            Some("calendarDate") => "invalid_calendar_date_operation",
                            Some("array") => "invalid_temporal_operation",
                            _ => "invalid_timestamp_operation",
                        };
                        return Err(CodecError::validation(
                            code,
                            format!("operation '{operation}' is not supported for temporal column '{field}'"),
                        ));
                    }
                    _ => {}
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
    fn temporal_arrays_reject_arithmetic_and_invalid_item_operations() {
        let schema = value!({"instants":{"type":"array","items":"timestamp"}});
        for mut patch in [
            value!({"instants":{"$inc":1}}),
            value!({"instants":{"$push":null}}),
            value!({"instants":{"$addToSet":"private_not_a_timestamp"}}),
            value!({"instants":{"$pull":"2026-02-30"}}),
        ] {
            let error = prepare_temporal_update(&schema, &mut patch).unwrap_err();
            assert!(!error.to_string().contains("private_not_a_timestamp"));
        }
    }

    #[test]
    fn temporal_nesting_is_bounded_and_json_null_remains_json() {
        let mut schema = value!({"type":"timestamp"});
        let mut data = value!(0);
        for _ in 0..=MAX_TEMPORAL_DEPTH {
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
