//! Schema-directed temporal values and typed JSON containers.
use super::{CodecError, Value};
use crate::schema::{ColumnSchema, FieldMap, LogicalType};

const MAX_TYPED_DEPTH: usize = super::MAX_JSON_DEPTH;

fn invalid(field: &str, expected: &str) -> CodecError {
    CodecError::validation(
        "invalid_typed_value",
        format!("column '{field}' requires {expected}"),
    )
}

/// Normalise a temporal value for storage.
///
/// A column keeps its instant at full resolution; the backend's registered
/// resolution decides what it can store. A value nested inside JSON becomes a
/// number of whole milliseconds, because that is the unit JSON carries, and a
/// finer one is refused rather than floored.
fn scalar(
    kind: LogicalType,
    field: &str,
    value: &mut Value,
    nested: bool,
) -> Result<(), CodecError> {
    if kind == LogicalType::CalendarDate {
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
        let micros = crate::sql::temporal::timestamp_micros(value).ok_or_else(|| {
            CodecError::validation(
                "invalid_timestamp",
                format!(
                    "column '{field}' requires a portable timestamp or integral Unix milliseconds"
                ),
            )
        })?;
        *value = if nested {
            let millis = crate::sql::temporal::exact_timestamp_millis(micros).ok_or_else(|| {
                CodecError::validation(
                    "timestamp_precision_unsupported",
                    format!("column '{field}' stores whole milliseconds inside JSON"),
                )
            })?;
            Value::from(millis)
        } else {
            Value::TimestampMicros(micros)
        };
    }
    Ok(())
}

fn vector(field: &str, definition: &ColumnSchema, value: &Value) -> Result<(), CodecError> {
    let dimensions = definition.vector_dims;
    let valid = value.as_array().is_some_and(|values| {
        !values.is_empty()
            && dimensions == Some(values.len())
            && values.iter().all(super::is_finite_vector_element)
    });
    if valid {
        Ok(())
    } else {
        Err(invalid(
            field,
            "a finite vector matching its declared dimensions",
        ))
    }
}

fn geographic_point(field: &str, value: &Value) -> Result<(), CodecError> {
    let valid = value.as_object().is_some_and(|point| {
        point
            .get("lat")
            .and_then(Value::as_f64)
            .is_some_and(|value| (-90.0..=90.0).contains(&value))
            && point
                .get("lng")
                .and_then(Value::as_f64)
                .is_some_and(|value| (-180.0..=180.0).contains(&value))
    });
    if valid {
        Ok(())
    } else {
        Err(invalid(
            field,
            "a geographic point within coordinate bounds",
        ))
    }
}

fn temporal_item(definition: &ColumnSchema) -> Option<LogicalType> {
    (definition.logical_type == LogicalType::Array)
        .then_some(definition.items)
        .flatten()
        .filter(|kind| matches!(kind, LogicalType::Timestamp | LogicalType::CalendarDate))
}

fn array_item(field: &str, definition: &ColumnSchema) -> Result<LogicalType, CodecError> {
    let item = definition.items.unwrap_or(LogicalType::Json);
    if matches!(
        item,
        LogicalType::Text
            | LogicalType::Number
            | LogicalType::Boolean
            | LogicalType::Timestamp
            | LogicalType::CalendarDate
            | LogicalType::Json
    ) {
        Ok(item)
    } else {
        Err(CodecError::validation(
            "invalid_array_item_type",
            format!("column '{field}' must declare a supported array item type"),
        ))
    }
}

fn invalid_element(field: &str, item: LogicalType) -> CodecError {
    CodecError::validation(
        "invalid_array_element",
        format!("column '{field}' requires {} array elements", item.as_str()),
    )
}

fn raw_element_matches(item: LogicalType, value: &serde_json::value::RawValue) -> bool {
    match item {
        LogicalType::Text => value.get().starts_with('"'),
        LogicalType::Number => value
            .get()
            .starts_with(|c: char| c == '-' || c.is_ascii_digit()),
        LogicalType::Boolean => matches!(value.get(), "true" | "false"),
        LogicalType::Json => true,
        _ => false,
    }
}

fn validate_encoded_array(field: &str, item: LogicalType, json: &str) -> Result<(), CodecError> {
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

/// Native text arrays hold strings only: SQL NULL elements have no ORM text
/// value, and PostgreSQL text cannot contain NUL. Both backends refuse the same
/// inputs.
fn native_text_element(field: &str, value: &Value) -> Result<(), CodecError> {
    match value {
        Value::String(text) if !text.contains('\0') => Ok(()),
        Value::String(_) => Err(CodecError::validation(
            "invalid_array_element",
            format!("column '{field}' requires string array elements without NUL characters"),
        )),
        _ => Err(invalid_element(field, LogicalType::Text)),
    }
}

fn native_text_array(field: &str, value: &mut Value) -> Result<(), CodecError> {
    if let Value::Json(json) = value {
        *value = serde_json::from_str(json).map_err(|_| invalid(field, "valid typed JSON"))?;
    }
    let values = value.as_array().ok_or_else(|| invalid(field, "an array"))?;
    for value in values {
        native_text_element(field, value)?;
    }
    Ok(())
}

fn prepare_array_element(
    field: &str,
    item: LogicalType,
    value: &mut Value,
) -> Result<(), CodecError> {
    if matches!(item, LogicalType::Timestamp | LogicalType::CalendarDate) {
        return scalar(item, field, value, true);
    }
    let valid = match value {
        Value::Json(json) => serde_json::from_str::<&serde_json::value::RawValue>(json)
            .is_ok_and(|raw| raw_element_matches(item, raw)),
        _ => match item {
            LogicalType::Text => value.is_string(),
            LogicalType::Number => value.is_number(),
            LogicalType::Boolean => value.is_boolean(),
            LogicalType::Json => true,
            _ => false,
        },
    };
    if !valid {
        return Err(invalid_element(field, item));
    }
    if item == LogicalType::Json {
        super::validate_json_value(field, value)?;
    }
    Ok(())
}

/// Prepare a field before storage or parameter binding. Untyped JSON is opaque.
///
/// # Errors
/// Refuses invalid typed values, containers, or excessive nesting.
pub fn prepare_value(
    field: &str,
    definition: &ColumnSchema,
    value: &mut Value,
) -> Result<(), CodecError> {
    prepare_value_at(field, definition, value, 0)
}

fn validate_nested_scalar(field: &str, kind: LogicalType, value: &Value) -> Result<(), CodecError> {
    let valid = match kind {
        LogicalType::Text => value.is_string(),
        LogicalType::Integer | LogicalType::BigInt => {
            matches!(value, Value::Number(number) if number.as_i64().is_some())
        }
        LogicalType::Number => value.is_number(),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid(field, kind.as_str()))
    }
}

fn prepare_value_at(
    field: &str,
    definition: &ColumnSchema,
    value: &mut Value,
    depth: usize,
) -> Result<(), CodecError> {
    if depth > MAX_TYPED_DEPTH {
        return Err(invalid(field, "bounded typed nesting"));
    }
    if value.is_null() {
        return Ok(());
    }
    if definition.has_native_array_storage() {
        return native_text_array(field, value);
    }
    let kind = definition.logical_type;
    if crate::sql::descriptors::is_exact_decimal(definition) {
        let input = match value {
            Value::Decimal(value) | Value::String(value) if crate::sql::decimal::valid(value) => {
                value.clone()
            }
            _ => return Err(invalid(field, "an exact decimal string")),
        };
        let storage = crate::sql::decimal::storage(definition)
            .map_err(|_| invalid(field, "valid fixed precision metadata"))?
            .ok_or_else(|| invalid(field, "valid fixed precision metadata"))?;
        let encoded = crate::sql::decimal::quantize(&input, storage)
            .map_err(|_| invalid(field, "an in-range exact decimal string"))?;
        *value = Value::Decimal(encoded);
        return Ok(());
    }
    if matches!(kind, LogicalType::Timestamp | LogicalType::CalendarDate) {
        return scalar(kind, field, value, depth > 0);
    }
    // JSON members have no column codec to enforce their primitive type.
    if depth > 0 {
        validate_nested_scalar(field, kind, value)?;
    }
    if kind == LogicalType::Boolean && !value.is_boolean() {
        return Err(invalid(field, "a boolean"));
    }
    if kind == LogicalType::Vector {
        return vector(field, definition, value);
    }
    if kind == LogicalType::GeoPoint {
        return geographic_point(field, value);
    }
    if kind == LogicalType::Json {
        return super::validate_json_value(field, value);
    }
    if !matches!(
        kind,
        LogicalType::Array | LogicalType::Object | LogicalType::Union
    ) {
        return Ok(());
    }
    if kind == LogicalType::Array {
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
    if kind == LogicalType::Array {
        let item = array_item(field, definition)?;
        let values = value
            .as_array_mut()
            .ok_or_else(|| invalid(field, "an array"))?;
        for value in values {
            prepare_array_element(field, item, value)?;
        }
        return Ok(());
    }
    let shape = if kind == LogicalType::Union {
        let discriminator = definition
            .discriminator
            .as_deref()
            .ok_or_else(|| invalid(field, "a union discriminator"))?;
        definition
            .variants
            .iter()
            .find(|variant| {
                value.get(discriminator).is_some_and(|actual| {
                    variant
                        .get(discriminator)
                        .and_then(|def| def.literal_value.as_ref())
                        == Some(actual)
                })
            })
            .ok_or_else(|| invalid(field, "a declared union variant"))?
    } else {
        &definition.shape
    };
    prepare_document_at(shape, value, field, depth + 1)
}

/// Validate and normalize writes before protection changes their storage shape.
///
/// # Errors
/// Refuses invalid typed values, containers, or excessive nesting.
pub fn prepare_document(schema: &FieldMap, document: &mut Value) -> Result<(), CodecError> {
    if !document.is_object() {
        return Ok(());
    }
    prepare_document_at(schema, document, "", 0)
}

fn prepare_document_at(
    schema: &FieldMap,
    document: &mut Value,
    prefix: &str,
    depth: usize,
) -> Result<(), CodecError> {
    let Some(document) = document.as_object_mut() else {
        return Err(invalid(prefix, "an object"));
    };
    for (field, value) in document {
        if let Some(definition) = schema.get(field) {
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
    definition: &ColumnSchema,
    value: &mut Value,
) -> Result<(), CodecError> {
    if definition.logical_type != LogicalType::Array {
        return Ok(());
    }
    if definition.has_native_array_storage() {
        return native_text_element(field, value);
    }
    prepare_array_element(field, array_item(field, definition)?, value)
}

/// Normalize typed assignments and array operations before protection.
///
/// # Errors
/// Refuses invalid typed operands and unsupported temporal operations.
pub fn prepare_update(schema: &FieldMap, patch: &mut Value) -> Result<(), CodecError> {
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
                    Operator::Increment | Operator::Decrement | Operator::Multiply
                        if crate::sql::descriptors::is_exact_decimal(definition) =>
                    {
                        prepare_value(field, definition, operand)?;
                    }
                    Operator::Increment | Operator::Decrement | Operator::Multiply => {
                        let valid = match definition.logical_type {
                            LogicalType::Integer | LogicalType::BigInt => {
                                matches!(operand, Value::Number(value) if value.as_i64().is_some())
                            }
                            LogicalType::Number => match operand {
                                Value::Number(_) => true,
                                Value::Decimal(value) => crate::sql::decimal::valid(value),
                                _ => false,
                            },
                            _ => false,
                        };
                        if !valid {
                            return Err(CodecError::validation(
                                "invalid_arithmetic_operand",
                                "arithmetic requires its assigned numeric column and operand",
                            ));
                        }
                    }
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
    macro_rules! column { ($($tokens:tt)*) => { ColumnSchema::from_descriptor(&value!($($tokens)*)).unwrap() }; }

    #[test]
    fn vector_and_geographic_writes_validate_the_declared_shape() {
        let vector = column!({"type":"vector","vectorDims":2});
        for mut value in [
            value!([]),
            value!([1.0]),
            value!([1.0, 2.0, 3.0]),
            value!([1.0, "two"]),
            Value::Array(vec![Value::try_from(f64::MAX).unwrap(), value!(0.0)]),
        ] {
            assert!(prepare_value("embedding", &vector, &mut value).is_err());
        }
        let mut valid = value!([1.0, -2.0]);
        prepare_value("embedding", &vector, &mut valid).unwrap();

        let point = column!({"type":"geoPoint"});
        for mut value in [
            value!({}),
            value!({"lat":0.0}),
            value!({"lat":"north","lng":0.0}),
            value!({"lat":91.0,"lng":0.0}),
            value!({"lat":0.0,"lng":181.0}),
        ] {
            assert!(prepare_value("location", &point, &mut value).is_err());
        }
        let mut valid = value!({"lat":90.0,"lng":-180.0});
        prepare_value("location", &point, &mut valid).unwrap();
    }

    #[test]
    fn exact_decimal_inputs_are_quantized_before_protection() {
        let definition = column!({"type":"number", "precision":30, "scale":2});
        let mut value = Value::String("9007199254740993.005".into());
        prepare_value("amount", &definition, &mut value).unwrap();
        assert_eq!(value, Value::Decimal("9007199254740993.01".into()));

        let mut overflow = Value::String("9999999999999999999999999999.995".into());
        assert!(prepare_value("amount", &definition, &mut overflow).is_err());
    }

    #[test]
    fn array_validation_preserves_native_buffers_and_encoded_numbers() {
        let definition = column!({"type":"array","items":"string"});
        let text = String::from("native input");
        let address = text.as_ptr();
        let mut data = Value::Array(vec![Value::String(text)]);
        prepare_value("names", &definition, &mut data).unwrap();
        assert_eq!(data[0].as_str().unwrap().as_ptr(), address);

        let json = "[1.00000000000000000000000001,18446744073709551616]";
        let mut data = Value::Json(json.into());
        prepare_value(
            "amounts",
            &column!({"type":"array","items":"number"}),
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
                &column!({"type":"array","items":item}),
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
            assert!(ColumnSchema::from_descriptor(&value!({"type":"array","items":item})).is_err());
        }
        let mut unsupported = ColumnSchema::new(LogicalType::Array);
        unsupported.items = Some(LogicalType::GeoPoint);
        assert!(matches!(
            prepare_value("values", &unsupported, &mut value!([])),
            Err(CodecError::Validation {
                code: "invalid_array_item_type",
                ..
            })
        ));
        for mut data in [
            value!([1, "text", null, [true]]),
            Value::Json("[1,\"text\",null,[true]]".into()),
        ] {
            prepare_value("values", &column!({"type":"array"}), &mut data).unwrap();
        }
    }

    #[test]
    fn documents_reject_storage_shaped_booleans_at_every_depth() {
        let schema = crate::schema::CollectionSchema::from_fields(&value!({
            "active": {"type":"boolean"},
            "settings": {
                "type":"object",
                "shape":{"enabled":{"type":"boolean"}}
            }
        }))
        .unwrap()
        .into_fields();
        for mut document in [
            value!({"active":1,"settings":{"enabled":true}}),
            value!({"active":true,"settings":{"enabled":1}}),
        ] {
            assert!(prepare_document(&schema, &mut document).is_err());
        }
    }

    #[test]
    fn temporal_arrays_reject_arithmetic_and_invalid_item_operations() {
        let schema = crate::schema::CollectionSchema::from_fields(
            &value!({"instants":{"type":"array","items":"timestamp"}}),
        )
        .unwrap()
        .into_fields();
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
        let mut schema = ColumnSchema::new(LogicalType::Timestamp);
        let mut data = value!(0);
        for _ in 0..=MAX_TYPED_DEPTH {
            let mut parent = ColumnSchema::new(LogicalType::Object);
            parent.shape.insert("child".into(), schema);
            schema = parent;
            data = value!({"child":data});
        }
        assert!(prepare_value("nested", &schema, &mut data).is_err());

        let mut data = Value::Json("null".into());
        prepare_value(
            "instants",
            &column!({"type":"array","items":"date"}),
            &mut data,
        )
        .unwrap();
        assert_eq!(data, Value::Json("null".into()));
    }
}
