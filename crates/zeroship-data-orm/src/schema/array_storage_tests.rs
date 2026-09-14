use super::*;
use crate::value;

fn descriptor(value: Value) -> Result<ColumnSchema, DbError> {
    ColumnSchema::from_descriptor(&value)
}

fn identity() -> ColumnSchema {
    let mut id = ColumnSchema::new(LogicalType::Text);
    id.primary_key = true;
    id
}

fn native(kind: LogicalType, items: Option<LogicalType>) -> ColumnSchema {
    let mut column = ColumnSchema::new(kind);
    column.items = items;
    column.storage.array = ArrayStorage::Native;
    column
}

fn validate(column: ColumnSchema) -> Result<(), DbError> {
    Schema::new([(
        "records".to_owned(),
        CollectionSchema::new([("id".to_owned(), identity()), ("labels".to_owned(), column)]),
    )])
    .validate()
}

#[test]
fn text_array_token_declares_native_text_storage() {
    for token in [
        value!({"type":"textArray", "required":true}),
        value!({"type":"textArray", "items":"string"}),
        value!({"type":"textArray", "items":"text"}),
    ] {
        let column = descriptor(token).unwrap();
        assert_eq!(column.logical_type, LogicalType::Array);
        assert_eq!(column.items, Some(LogicalType::Text));
        assert_eq!(column.storage.array, ArrayStorage::Native);
        assert!(column.has_native_array_storage());
    }
    let mapped =
        descriptor(value!({"type":"textArray", "storage":{"valueColumn":"stored_labels"}}))
            .unwrap();
    assert_eq!(mapped.storage.array, ArrayStorage::Native);
    assert_eq!(
        mapped.storage.value_column.as_deref(),
        Some("stored_labels")
    );

    let json = descriptor(value!({"type":"array", "items":"string"})).unwrap();
    assert_eq!(json.logical_type, LogicalType::Array);
    assert_eq!(json.storage.array, ArrayStorage::Json);
    assert!(!json.has_native_array_storage());
}

#[test]
fn text_array_token_refuses_conflicting_or_unknown_spellings() {
    for rejected in [
        value!({"type":"textArray", "items":"number"}),
        value!({"type":"textArray", "items":"json"}),
        value!({"type":"integerArray"}),
        value!({"type":"array", "items":"textArray"}),
    ] {
        let error = descriptor(rejected.clone()).unwrap_err();
        assert!(
            matches!(
                error,
                DbError::ValidationFailed {
                    code: "invalid_schema",
                    ..
                }
            ),
            "{rejected}: {error:?}"
        );
    }
    assert!(descriptor(value!({"type":"textArray"})).is_ok());
}

#[test]
fn schema_validation_limits_native_storage_to_unprotected_top_level_text_arrays() {
    validate(native(LogicalType::Array, Some(LogicalType::Text))).unwrap();
    let mut nullable = native(LogicalType::Array, Some(LogicalType::Text));
    nullable.required = false;
    validate(nullable).unwrap();
    validate(descriptor(value!({"type":"textArray", "default":[]})).unwrap()).unwrap();

    let mut encrypted = native(LogicalType::Array, Some(LogicalType::Text));
    encrypted.encrypted = true;
    let mut masked = native(LogicalType::Array, Some(LogicalType::Text));
    masked.mask = Some(MaskSchema {
        kind: "full".into(),
        classification: "pii".into(),
    });
    let mut nested = ColumnSchema::new(LogicalType::Object);
    nested.shape.insert(
        "labels".into(),
        native(LogicalType::Array, Some(LogicalType::Text)),
    );
    let mut rejected = vec![
        native(LogicalType::Array, None),
        native(LogicalType::Text, None),
        native(LogicalType::Json, None),
        native(LogicalType::Object, None),
        encrypted,
        masked,
        nested,
    ];
    for item in [
        LogicalType::Number,
        LogicalType::Boolean,
        LogicalType::Timestamp,
        LogicalType::CalendarDate,
        LogicalType::Json,
    ] {
        rejected.push(native(LogicalType::Array, Some(item)));
    }
    for column in rejected {
        let description = format!("{column:?}");
        let error = validate(column).unwrap_err();
        assert!(
            matches!(
                error,
                DbError::ValidationFailed {
                    code: "invalid_schema",
                    ..
                }
            ),
            "{description}: {error:?}"
        );
    }
}
