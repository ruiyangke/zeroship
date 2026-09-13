use zeroship_data_orm::{
    schema::{
        Assignment, AssignmentEvent, AssignmentGenerator, CollectionSchema, ColumnSchema,
        LogicalType, MaskSchema, RelationSchema, Schema,
    },
    value,
    value::Value,
};

fn identity() -> ColumnSchema {
    let mut column = ColumnSchema::new(LogicalType::Text);
    column.primary_key = true;
    column
}

#[test]
fn artifact_and_native_declarations_have_the_same_contract() {
    let mut timestamp = ColumnSchema::new(LogicalType::Timestamp);
    timestamp.assignment = Some(Assignment {
        by: AssignmentGenerator::Now,
        on: AssignmentEvent::Insert,
    });
    let mut nickname = ColumnSchema::new(LogicalType::Text);
    nickname.required = false;
    let native = CollectionSchema::new([
        ("id".into(), identity()),
        ("nickname".into(), nickname),
        ("happened".into(), timestamp),
    ]);
    let artifact = CollectionSchema::from_fields(&value!({
        "happened": {
            "type":"date", "required":true, "writable":false,
            "storage":{"valueColumn":"happened"},
            "assign":{"by":"now", "on":"insert"}
        },
        "nickname": {"type":"string", "readable":true, "mask":{"kind":"none"}},
        "id": {"type":"text", "required":true, "primaryKey":true}
    }))
    .unwrap();
    assert_eq!(native, artifact);
}

#[test]
fn canonicalization_preserves_changed_contracts() {
    let baseline = ColumnSchema::new(LogicalType::Text);
    let mut nullable = baseline.clone();
    nullable.required = false;
    let mut protected = baseline.clone();
    protected.mask = Some(MaskSchema {
        kind: "full".into(),
        classification: "pii".into(),
    });
    let mut hidden = baseline.clone();
    hidden.projectable = false;
    for changed in [nullable, protected, hidden] {
        assert_ne!(baseline, changed);
    }
    assert_ne!(
        ColumnSchema::new(LogicalType::Integer),
        ColumnSchema::new(LogicalType::BigInt)
    );
    let mut explicit_null = baseline.clone();
    explicit_null.default = Some(value!(null));
    assert_ne!(baseline, explicit_null);
}

#[test]
fn registration_validates_identity_and_declared_generator_roles() {
    let invalid = Schema::new([(
        "posts".into(),
        CollectionSchema::new([("title".into(), ColumnSchema::new(LogicalType::Text))]),
    )]);
    assert!(invalid.validate().is_err());

    let mut marker = ColumnSchema::new(LogicalType::Timestamp);
    marker.required = false;
    marker.soft_delete = true;
    let invalid = Schema::new([(
        "posts".into(),
        CollectionSchema::new([
            ("id".into(), identity()),
            ("removed".into(), marker.clone()),
        ]),
    )]);
    assert!(invalid.validate().is_err());
    marker.assignment = Some(Assignment {
        by: AssignmentGenerator::Now,
        on: AssignmentEvent::Delete,
    });
    Schema::new([(
        "posts".into(),
        CollectionSchema::new([("id".into(), identity()), ("removed".into(), marker)]),
    )])
    .validate()
    .unwrap();
}

#[test]
fn relation_graph_uses_declared_targets_and_logical_key_types() {
    let mut owner = ColumnSchema::new(LogicalType::Text);
    owner.reference = Some(RelationSchema {
        collection: "authors".into(),
        column: "id".into(),
        name: Some("author".into()),
    });
    let posts = CollectionSchema::new([("id".into(), identity()), ("author_id".into(), owner)]);
    assert!(Schema::new([("posts".into(), posts.clone())])
        .validate()
        .is_err());
    let authors = CollectionSchema::new([("id".into(), identity())]);
    Schema::new([("posts".into(), posts), ("authors".into(), authors)])
        .validate()
        .unwrap();
}

#[test]
fn nested_fields_keep_typed_json_semantics_without_entity_identity() {
    let fields = CollectionSchema::from_fields(&value!({
        "payload": {
            "type":"object",
            "shape": {
                "dates": {"type":"array", "items":"calendarDate"},
                "occurred": {"type":"timestamp"}
            }
        }
    }))
    .unwrap();
    assert_eq!(fields["payload"].logical_type, LogicalType::Object);
    assert_eq!(
        fields["payload"].shape["dates"].items,
        Some(LogicalType::CalendarDate)
    );
    assert_eq!(
        fields["payload"].shape["occurred"].logical_type,
        LogicalType::Timestamp
    );
}

#[test]
fn registration_rejects_duplicate_native_field_declarations() {
    let fields = CollectionSchema::new([
        ("id".into(), identity()),
        ("title".into(), ColumnSchema::new(LogicalType::Text)),
        ("title".into(), ColumnSchema::new(LogicalType::Integer)),
    ]);
    assert!(Schema::new([("posts".into(), fields)]).validate().is_err());
    Schema::new([(
        "posts".into(),
        CollectionSchema::new([("id".into(), identity())]),
    )])
    .validate()
    .unwrap();
}

#[test]
fn defaults_share_the_logical_value_contract_across_frontends() {
    let mut amount = ColumnSchema::new(LogicalType::Number);
    amount.precision = Some(18);
    amount.scale = Some(2);
    amount.default = Some(zeroship_data_orm::value::Value::Decimal("12.50".into()));
    let mut occurred = ColumnSchema::new(LogicalType::Timestamp);
    occurred.default = Some(zeroship_data_orm::value::Value::Timestamp(0));
    let native = CollectionSchema::new([("amount".into(), amount), ("occurred".into(), occurred)]);
    let artifact = value!({
        "amount":{"type":"number", "required":true, "precision":18, "scale":2, "default":"12.5"},
        "occurred":{"type":"timestamp", "required":true, "default":"1970-01-01T00:00:00Z"}
    });
    assert_eq!(native, CollectionSchema::from_fields(&artifact).unwrap());
    let mut different = artifact;
    different["amount"]["default"] = value!("13.50");
    assert_ne!(native, CollectionSchema::from_fields(&different).unwrap());
}

#[test]
fn union_discriminators_must_select_a_unique_literal_variant() {
    fn union(tags: &[(&str, LogicalType)]) -> Schema {
        let mut payload = ColumnSchema::new(LogicalType::Union);
        payload.discriminator = Some("kind".into());
        payload.variants = tags
            .iter()
            .map(|(tag, kind)| {
                let mut column = ColumnSchema::new(*kind);
                column.literal_value = Some(value!(*tag));
                [("kind".into(), column)].into()
            })
            .collect();
        Schema::new([(
            "events".into(),
            CollectionSchema::new([("id".into(), identity()), ("payload".into(), payload)]),
        )])
    }
    union(&[("one", LogicalType::Literal), ("two", LogicalType::Literal)])
        .validate()
        .unwrap();
    assert!(
        union(&[("one", LogicalType::Literal), ("one", LogicalType::Literal)])
            .validate()
            .is_err()
    );
    assert!(union(&[("one", LogicalType::Text)]).validate().is_err());
}

fn schema_with_default(kind: LogicalType, required: bool, default: Value) -> Schema {
    let mut column = ColumnSchema::new(kind);
    column.required = required;
    column.default = Some(default);
    Schema::new([(
        "settings".into(),
        CollectionSchema::new([("id".into(), identity()), ("value".into(), column)]),
    )])
}

#[test]
fn scalar_defaults_require_values_of_the_declared_logical_type() {
    for (kind, valid, invalid) in [
        (
            LogicalType::Text,
            value!("text"),
            vec![value!(true), value!(12)],
        ),
        (
            LogicalType::Integer,
            Value::from(i64::MIN),
            vec![value!("12"), value!(1.5), Value::from(u64::MAX)],
        ),
        (
            LogicalType::BigInt,
            Value::from(i64::MAX),
            vec![value!(false), value!(1.0), Value::from(u64::MAX)],
        ),
        (
            LogicalType::Number,
            value!(1.5),
            vec![
                value!("1.5"),
                value!(true),
                Value::Decimal("invalid".into()),
            ],
        ),
        (
            LogicalType::Boolean,
            value!(true),
            vec![value!(1), value!("true")],
        ),
        (
            LogicalType::Bytes,
            Value::Bytes(vec![0, 255]),
            vec![value!("bytes"), value!([0, 255])],
        ),
        (LogicalType::Time, value!("12:30:00"), vec![value!(123)]),
    ] {
        schema_with_default(kind, true, valid).validate().unwrap();
        for default in invalid {
            for required in [false, true] {
                let result = schema_with_default(kind, required, default.clone()).validate();
                assert!(result.is_err(), "{kind:?} accepted {default:?}");
                assert!(
                    matches!(result.unwrap_err(), zeroship_data_orm::error::DbError::ValidationFailed { code, .. } if code == "invalid_schema")
                );
            }
        }
    }
    schema_with_default(LogicalType::Number, true, Value::Decimal("12.5".into()))
        .validate()
        .unwrap();
}

#[test]
fn scalar_defaults_distinguish_required_null_from_nullable_values() {
    for kind in [
        LogicalType::Text,
        LogicalType::Integer,
        LogicalType::BigInt,
        LogicalType::Number,
        LogicalType::Boolean,
        LogicalType::Bytes,
        LogicalType::Timestamp,
        LogicalType::CalendarDate,
        LogicalType::Time,
        LogicalType::Json,
        LogicalType::Object,
        LogicalType::Array,
    ] {
        schema_with_default(kind, false, Value::Null)
            .validate()
            .unwrap();
        assert!(
            schema_with_default(kind, true, Value::Null)
                .validate()
                .is_err(),
            "{kind:?}"
        );
    }
    for kind in [LogicalType::Json, LogicalType::Object, LogicalType::Array] {
        schema_with_default(kind, true, Value::Json("null".into()))
            .validate()
            .unwrap();
    }
}

#[test]
fn scalar_defaults_preserve_native_bytes_and_temporal_values() {
    let bytes = schema_with_default(LogicalType::Bytes, true, Value::Bytes(vec![0, 255]));
    bytes.validate().unwrap();
    let (_, fields) = bytes.collections().next().unwrap();
    assert_eq!(fields["value"].default, Some(Value::Bytes(vec![0, 255])));

    schema_with_default(LogicalType::Timestamp, true, Value::Timestamp(0))
        .validate()
        .unwrap();
    schema_with_default(LogicalType::CalendarDate, true, value!("2026-09-13"))
        .validate()
        .unwrap();
    for (kind, value) in [
        (LogicalType::Timestamp, value!("invalid")),
        (LogicalType::CalendarDate, value!("2026-02-30")),
    ] {
        assert!(schema_with_default(kind, true, value).validate().is_err());
    }
}

#[test]
fn artifact_binary_defaults_decode_into_native_metadata() {
    for (encoded, bytes) in [("AP8=", vec![0, 255]), ("", vec![])] {
        let artifact = Schema::from_runtime_descriptor(&value!({
            "version": 2,
            "collections": {"settings": {"fields": {
                "id": {"type": "string", "required": true, "primaryKey": true},
                "value": {"type": "bytes", "required": true, "default": encoded}
            }}}
        }))
        .unwrap();
        artifact.validate().unwrap();
        assert_eq!(
            artifact,
            schema_with_default(LogicalType::Bytes, true, Value::Bytes(bytes))
        );
    }

    assert!(Schema::from_runtime_descriptor(&value!({
        "version": 2,
        "collections": {"settings": {"fields": {
            "id": {"type": "string", "required": true, "primaryKey": true},
            "value": {"type": "bytes", "default": "invalid base64!"}
        }}}
    }))
    .is_err());
}

#[test]
fn conflicting_physical_storage_cannot_replace_an_installed_schema() {
    use zeroship_data_orm::{binding::DbBinding, descriptor, OrmContext};

    let context = OrmContext::new();
    context.with(|| {
        let binding = DbBinding::new(
            "app_storage_contract",
            "revision",
            zeroship_data_orm::sql::SchemaName::new("storage_contract").unwrap(),
        );
        let original = CollectionSchema::new([("id".into(), identity())]);
        descriptor::install_collections(
            &binding,
            Schema::new([("posts".into(), original.clone())]),
        )
        .unwrap();

        for fields in [
            value!({
                "id": {"type":"string", "required":true, "primaryKey":true},
                "title": {"type":"string", "storage":{"valueColumn":"id"}}
            }),
            value!({
                "id": {"type":"string", "required":true, "primaryKey":true},
                "secret": {"type":"string", "mask":{"kind":"full"}},
                "other": {"type":"string", "mask":{"kind":"full"},
                    "storage":{"rawColumn":"__zs_raw__secret"}}
            }),
            value!({
                "id": {"type":"string", "required":true, "primaryKey":true},
                "secret": {"type":"string", "mask":{"kind":"full"}},
                "public": {"type":"string", "storage":{"valueColumn":"__zs_raw__secret"}}
            }),
        ] {
            let artifact = CollectionSchema::from_fields(&fields).unwrap();
            let native = CollectionSchema::new(artifact.clone().into_fields());
            for candidate in [artifact, native] {
                assert!(descriptor::install_collections(
                    &binding,
                    Schema::new([("posts".into(), candidate)]),
                )
                .is_err());
                assert_eq!(
                    descriptor::collection_schema(&binding, "posts")
                        .unwrap()
                        .as_ref(),
                    original.fields(),
                );
            }
        }

        let mut title = ColumnSchema::new(LogicalType::Text);
        title.storage.value_column = Some("stored_title".into());
        descriptor::install_collections(
            &binding,
            Schema::new([(
                "posts".into(),
                CollectionSchema::new([("id".into(), identity()), ("title".into(), title)]),
            )]),
        )
        .unwrap();
    });
}
