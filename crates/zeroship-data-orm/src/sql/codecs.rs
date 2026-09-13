//! Logical values and their database storage representations.
use crate::sql::mapping;
use crate::value::Value;

mod typed;
use crate::schema::{ColumnSchema, FieldMap, LogicalType};
pub(crate) use typed::prepare_value;
pub use typed::{prepare_document, prepare_update};

pub(crate) const MAX_JSON_DEPTH: usize = 128;

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    Internal {
        message: String,
    },
    Validation {
        code: &'static str,
        message: String,
    },
    Decode {
        column: String,
        reason: &'static str,
    },
}
impl CodecError {
    fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }
    pub(crate) fn validation(code: &'static str, message: impl Into<String>) -> Self {
        Self::Validation {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal { message } | Self::Validation { message, .. } => f.write_str(message),
            Self::Decode { column, reason } => {
                write!(f, "cannot decode column '{column}': {reason}")
            }
        }
    }
}
impl std::error::Error for CodecError {}

pub(crate) fn validate_json_value(field: &str, value: &Value) -> Result<(), CodecError> {
    let mut pending = vec![(value, 0usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_JSON_DEPTH {
            return Err(CodecError::validation(
                "invalid_json_value",
                format!("column '{field}' requires bounded JSON nesting"),
            ));
        }
        match value {
            Value::Json(encoded) => {
                validate_encoded_json_depth(field, encoded)?;
                serde_json::from_str::<&serde_json::value::RawValue>(encoded).map_err(|_| {
                    CodecError::validation(
                        "invalid_json_value",
                        format!("column '{field}' requires valid JSON"),
                    )
                })?;
            }
            Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_encoded_json_depth(field: &str, encoded: &str) -> Result<(), CodecError> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in encoded.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_JSON_DEPTH {
                    return Err(CodecError::validation(
                        "invalid_json_value",
                        format!("column '{field}' requires bounded JSON nesting"),
                    ));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// Convert driver results to logical values through the selected storage codecs.
pub(crate) fn decode_rows(
    registration: &crate::sql::registration::SqlRegistration,
    schema: &FieldMap,
    rows: &mut [Value],
) -> Result<(), CodecError> {
    for row in rows.iter_mut() {
        normalize_row_on_read(registration, schema, row)?;
    }
    Ok(())
}

fn normalize_row_on_read(
    registration: &crate::sql::registration::SqlRegistration,
    schema: &FieldMap,
    row: &mut Value,
) -> Result<(), CodecError> {
    let Some(obj) = row.as_object_mut() else {
        return Ok(());
    };
    for (key, value) in obj.iter_mut() {
        let Some(def) = schema.get(key) else {
            continue;
        };

        // Protected storage is decoded by the protection pipeline. A mask is
        // stored as text even when its logical field is numeric or binary.
        if def.encrypted || mapping::column_is_masked(key, schema) {
            continue;
        }

        let storage = registration
            .storage_type(&schema[key])
            .map_err(|_| CodecError::Decode {
                column: key.clone(),
                reason: "unsupported storage type",
            })?;
        let stored = std::mem::replace(value, Value::Null);
        *value = registration
            .decode(storage, stored)
            .map_err(|_| CodecError::Decode {
                column: key.clone(),
                reason: "invalid storage value",
            })?;

        match def.logical_type {
            LogicalType::Boolean => normalize_boolean_value(value)?,
            LogicalType::Json | LogicalType::Object | LogicalType::Array | LogicalType::Union => {
                prepare_value(key, &schema[key], value).map_err(|_| CodecError::Decode {
                    column: key.clone(),
                    reason: "invalid typed JSON storage",
                })?;
            }
            LogicalType::Bytes => normalize_bytes_value(value)?,
            LogicalType::Timestamp => normalize_timestamp_value(key, value)?,
            LogicalType::CalendarDate => {
                if !value.is_null()
                    && value.as_str().is_none_or(|date| {
                        crate::sql::temporal::parse_calendar_date(date).is_none()
                    })
                {
                    return Err(CodecError::Decode {
                        column: key.clone(),
                        reason: "invalid calendar date storage",
                    });
                }
            }
            LogicalType::Vector => normalize_vector_value(key, def, value)?,
            LogicalType::GeoPoint => normalize_geopoint_value(key, value)?,
            _ => {}
        }
    }
    Ok(())
}

fn normalize_vector_value(
    field: &str,
    definition: &ColumnSchema,
    value: &mut Value,
) -> Result<(), CodecError> {
    let invalid = || CodecError::internal(format!("invalid vector storage for column '{field}'"));
    if let Value::Bytes(bytes) = value {
        if bytes.is_empty() || bytes.len() % 4 != 0 {
            return Err(invalid());
        }
        let values = bytes
            .chunks_exact(4)
            .map(|bytes| {
                let number = f32::from_le_bytes(bytes.try_into().unwrap());
                Value::try_from(f64::from(number)).map_err(|_| invalid())
            })
            .collect::<Result<Vec<_>, _>>()?;
        *value = Value::Array(values);
    }
    match value {
        Value::Null => Ok(()),
        Value::Array(values) => {
            let dimensions = definition.vector_dims;
            if values.is_empty()
                || dimensions != Some(values.len())
                || values
                    .iter()
                    .any(|v| !v.as_f64().is_some_and(|n| (n as f32).is_finite()))
            {
                return Err(invalid());
            }
            Ok(())
        }
        _ => Err(invalid()),
    }
}

fn normalize_geopoint_value(field: &str, value: &mut Value) -> Result<(), CodecError> {
    let invalid = || {
        CodecError::internal(format!(
            "invalid geographic point storage for column '{field}'"
        ))
    };
    if let Value::Bytes(bytes) = value {
        if bytes.len() != 16 {
            return Err(invalid());
        }
        let lat = f64::from_le_bytes(bytes[..8].try_into().unwrap());
        let lng = f64::from_le_bytes(bytes[8..].try_into().unwrap());
        let lat = Value::try_from(lat).map_err(|_| invalid())?;
        let lng = Value::try_from(lng).map_err(|_| invalid())?;
        *value = Value::Object([("lat".into(), lat), ("lng".into(), lng)].into());
    }
    match value {
        Value::Null => Ok(()),
        Value::Object(point)
            if point
                .get("lat")
                .and_then(Value::as_f64)
                .is_some_and(|v| (-90.0..=90.0).contains(&v))
                && point
                    .get("lng")
                    .and_then(Value::as_f64)
                    .is_some_and(|v| (-180.0..=180.0).contains(&v)) =>
        {
            Ok(())
        }
        _ => Err(invalid()),
    }
}

fn normalize_boolean_value(value: &mut Value) -> Result<(), CodecError> {
    match value {
        Value::Bool(_) | Value::Null => Ok(()),
        Value::Number(n) => {
            if n.as_i64() == Some(0) {
                *value = Value::Bool(false);
                Ok(())
            } else if n.as_i64() == Some(1) {
                *value = Value::Bool(true);
                Ok(())
            } else {
                Err(CodecError::internal(format!(
                    "normalize_row_on_read: boolean field expected 0/1, got {n}"
                )))
            }
        }
        Value::String(s) => match s.as_str() {
            "0" | "false" => {
                *value = Value::Bool(false);
                Ok(())
            }
            "1" | "true" => {
                *value = Value::Bool(true);
                Ok(())
            }
            other => Err(CodecError::internal(format!(
                "normalize_row_on_read: boolean field expected 0/1/true/false, got {other:?}"
            ))),
        },
        other => Err(CodecError::internal(format!(
            "normalize_row_on_read: boolean field expected bool/string/number/null, got {other:?}"
        ))),
    }
}

fn normalize_bytes_value(value: &mut Value) -> Result<(), CodecError> {
    match value {
        Value::Null | Value::Bytes(_) => Ok(()),
        _ => Err(CodecError::internal(
            "bytes column did not return native bytes",
        )),
    }
}

fn normalize_timestamp_value(field: &str, value: &mut Value) -> Result<(), CodecError> {
    if value.is_null() {
        return Ok(());
    }
    let millis =
        crate::sql::temporal::timestamp_millis(value).ok_or_else(|| CodecError::Decode {
            column: field.to_string(),
            reason: "invalid timestamp storage",
        })?;
    *value = Value::Timestamp(millis);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sqlite_registration_packs_vector_and_geopoint_values() {
        let schema = crate::schema::CollectionSchema::from_fields(&crate::value!({
            "embedding": { "type": "vector", "vectorDims": 4 },
            "loc": { "type": "geoPoint" },
            "name": { "type": "string" }
        }))
        .unwrap()
        .into_fields();
        let registration = crate::sql::registration::SqlRegistration::sqlite();
        let embedding = registration
            .storage_type(&schema["embedding"])
            .and_then(|storage| registration.encode(storage, crate::value!([1.0, 0.0, 0.5, -1.25])))
            .expect("encode vector");
        let location = registration
            .storage_type(&schema["loc"])
            .and_then(|storage| {
                registration.encode(storage, crate::value!({"lat":37.7749,"lng":-122.4194}))
            })
            .expect("encode geographic point");

        let embedding_bytes = embedding.as_bytes().expect("native vector buffer");
        let loc_bytes = location.as_bytes().expect("native geography buffer");

        assert_eq!(
            embedding_bytes,
            crate::sql::sqlite_values::vec_to_le_bytes(&[1.0, 0.0, 0.5, -1.25]),
        );
        assert_eq!(
            loc_bytes,
            crate::sql::sqlite_values::point_to_blob(crate::sql::descriptors::GeoPoint {
                lat: 37.7749,
                lng: -122.4194,
            }),
        );
    }
    #[test]
    fn normalize_row_on_read_coerces_sqlite_wire_shapes() {
        let schema = crate::schema::CollectionSchema::from_fields(&crate::value!({
            "active": { "type": "boolean" },
            "prefs": { "type": "object" },
            "avatar": { "type": "bytes" },
            "published_at": { "type": "date" }
        }))
        .unwrap()
        .into_fields();
        let mut row = crate::value!({
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}",
            "avatar": Value::Bytes(vec![104, 105]),
            "published_at": "2026-05-07T01:02:03.004Z"
        });

        normalize_row_on_read(
            &crate::sql::registration::SqlRegistration::sqlite(),
            &schema,
            &mut row,
        )
        .expect("normalize");

        assert_eq!(row["active"], Value::Bool(true));
        assert_eq!(row["prefs"], crate::value!({"theme":"dark"}));
        assert_eq!(row["avatar"], Value::Bytes(vec![104, 105]));
        assert_eq!(row["published_at"], Value::Timestamp(1_778_115_723_004));
    }
    #[test]
    fn normalize_row_on_read_skips_encrypted_columns() {
        let schema = crate::schema::CollectionSchema::from_fields(&crate::value!({
            "secret": {
                "type": "bytes",
                "encrypted": true
            }
        }))
        .unwrap()
        .into_fields();
        let mut row = crate::value!({
            "secret": "AQID"
        });

        normalize_row_on_read(
            &crate::sql::registration::SqlRegistration::sqlite(),
            &schema,
            &mut row,
        )
        .expect("normalize");

        assert_eq!(row["secret"], Value::String("AQID".to_string()));
    }
    #[test]
    fn undeclared_names_do_not_select_a_codec() {
        let mut row = crate::value!({
            "created_at": "2026-05-07T01:02:03.004Z",
            "published_at": "2026-05-07T01:02:03.004Z",
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}"
        });

        normalize_row_on_read(
            &crate::sql::registration::SqlRegistration::sqlite(),
            &crate::sql::mapping::empty_read_schema(),
            &mut row,
        )
        .expect("normalize");

        assert_eq!(
            row["created_at"],
            Value::String("2026-05-07T01:02:03.004Z".to_string())
        );
        assert_eq!(
            row["published_at"],
            Value::String("2026-05-07T01:02:03.004Z".to_string())
        );
        assert_eq!(row["active"], crate::value!(1));
        assert_eq!(
            row["prefs"],
            Value::String("{\"theme\":\"dark\"}".to_string())
        );
    }
    #[test]
    fn declared_types_override_familiar_column_names() {
        let schema = crate::schema::CollectionSchema::from_fields(&crate::value!({
            "created_at":{"type":"string"},
            "updated_at":{"type":"int"},
            "occurred_at":{"type":"date"}
        }))
        .unwrap()
        .into_fields();
        let mut rows = vec![
            crate::value!({"created_at":"ordinary text", "updated_at":9, "occurred_at":"2026-05-07T01:02:03.004Z"}),
        ];
        decode_rows(
            &crate::sql::registration::SqlRegistration::sqlite(),
            &schema,
            &mut rows,
        )
        .unwrap();
        assert_eq!(rows[0]["created_at"], crate::value!("ordinary text"));
        assert_eq!(rows[0]["updated_at"], crate::value!(9));
        assert_eq!(rows[0]["occurred_at"], Value::Timestamp(1_778_115_723_004));
        let registration = crate::sql::registration::SqlRegistration::postgres();
        let storage = registration.storage_type(&schema["created_at"]).unwrap();
        assert_eq!(storage, crate::sql::statement::StorageType::Text);
        assert_eq!(
            registration
                .encode(storage, crate::value!("ordinary text"))
                .unwrap(),
            crate::value!("ordinary text")
        );
    }
    #[test]
    fn parse_timestamp_millis_accepts_iso_z_and_variable_fraction() {
        let expected = 1_746_579_723_004i64;
        assert_eq!(
            crate::sql::temporal::parse_timestamp_millis("2025-05-07T01:02:03.004Z"),
            Some(expected)
        );
        assert_eq!(
            crate::sql::temporal::parse_timestamp_millis("2025-05-07T01:02:03.004999Z"),
            Some(expected)
        );
        assert_eq!(
            crate::sql::temporal::parse_timestamp_millis("2025-05-07T03:02:03.004+02:00"),
            Some(expected)
        );
    }
    #[test]
    fn normalize_row_on_read_rejects_out_of_domain_boolean_values() {
        let schema = crate::schema::CollectionSchema::from_fields(&crate::value!({
            "active": { "type": "boolean" }
        }))
        .unwrap()
        .into_fields();
        let mut row = crate::value!({
            "active": 2
        });

        let err = normalize_row_on_read(
            &crate::sql::registration::SqlRegistration::sqlite(),
            &schema,
            &mut row,
        )
        .expect_err("declared boolean field must reject out-of-domain values");
        match err {
            CodecError::Internal { message } => {
                assert!(
                    message.contains("boolean field expected 0/1"),
                    "error should explain the boolean domain violation: {message}"
                );
            }
            other => panic!("expected Internal error, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;
    use crate::value;

    #[test]
    fn timestamp_aliases_validate_storage_without_echoing_values() {
        for kind in ["date", "timestamp"] {
            let schema =
                crate::schema::CollectionSchema::from_fields(&value!({"instant":{"type":kind}}))
                    .unwrap()
                    .into_fields();
            for registration in [
                crate::sql::registration::SqlRegistration::postgres(),
                crate::sql::registration::SqlRegistration::sqlite(),
            ] {
                for value in [
                    value!(-1),
                    value!(-1.0),
                    Value::Timestamp(-1),
                    value!("1969-12-31T23:59:59.999999Z"),
                ] {
                    let mut rows = [value!({"instant":value})];
                    decode_rows(&registration, &schema, &mut rows).unwrap();
                    assert_eq!(rows[0]["instant"], Value::Timestamp(-1));
                }
                for value in [
                    value!("private_invalid_instant"),
                    value!("2026-02-30"),
                    value!(0.5),
                    value!(true),
                    value!({"secret":"private_invalid_instant"}),
                    Value::Timestamp(i64::MAX),
                ] {
                    let mut rows = [value!({"instant":value})];
                    let error = decode_rows(&registration, &schema, &mut rows).unwrap_err();
                    assert_eq!(
                        error,
                        CodecError::Decode {
                            column: "instant".into(),
                            reason: "invalid timestamp storage"
                        }
                    );
                    assert!(!error.to_string().contains("private_invalid_instant"));
                }
            }
        }
    }

    #[test]
    fn protected_fields_keep_their_storage_shape_until_protection_decodes_them() {
        let schema = crate::schema::CollectionSchema::from_fields(&value!({
            "masked":{"type":"date", "mask":{"kind":"full"}},
            "encrypted":{"type":"string", "encrypted":true},
        }))
        .unwrap()
        .into_fields();
        let original = value!({"masked":"***", "encrypted":Value::Bytes(vec![1, 2, 3])});
        let mut rows = [original.clone()];
        decode_rows(
            &crate::sql::registration::SqlRegistration::sqlite(),
            &schema,
            &mut rows,
        )
        .unwrap();
        assert_eq!(rows[0], original);
    }
}

#[cfg(test)]
mod calendar_date_tests {
    use super::*;
    use crate::value;

    #[test]
    fn calendar_date_reads_reject_invalid_storage_without_echoing_it() {
        let schema = crate::schema::CollectionSchema::from_fields(
            &value!({"birthday":{"type":"calendarDate"}}),
        )
        .unwrap()
        .into_fields();
        for stored in [
            value!("2026-02-30"),
            value!("1900-02-29"),
            value!("0000-01-01"),
            value!("2026-01-01T00:00:00Z"),
            value!("private_not_a_date"),
            value!(0),
        ] {
            let mut rows = [value!({"birthday":stored})];
            let error = decode_rows(
                &crate::sql::registration::SqlRegistration::sqlite(),
                &schema,
                &mut rows,
            )
            .unwrap_err();
            assert!(error.to_string().contains("birthday"));
            assert!(!error.to_string().contains("private_not_a_date"));
        }
    }
}

#[cfg(test)]
mod json_read_tests {
    use super::*;
    use crate::value;

    #[test]
    fn native_json_strings_remain_strings() {
        let schema =
            crate::schema::CollectionSchema::from_fields(&value!({"payload":{"type":"json"}}))
                .unwrap()
                .into_fields();
        for text in ["true", "null", "42", "[1]", "{\"key\":1}", "\"nested\""] {
            let mut rows = [value!({"payload":text})];
            decode_rows(
                &crate::sql::registration::SqlRegistration::postgres(),
                &schema,
                &mut rows,
            )
            .unwrap();
            assert_eq!(rows[0]["payload"], Value::String(text.into()));
        }
    }

    #[test]
    fn sqlite_json_text_requires_valid_json() {
        let schema =
            crate::schema::CollectionSchema::from_fields(&value!({"payload":{"type":"json"}}))
                .unwrap()
                .into_fields();
        let mut rows = [value!({"payload":"secret_invalid_json"})];
        let error = decode_rows(
            &crate::sql::registration::SqlRegistration::sqlite(),
            &schema,
            &mut rows,
        )
        .unwrap_err();
        assert!(error.to_string().contains("payload"));
        assert!(!error.to_string().contains("secret_invalid_json"));
    }

    #[test]
    fn encoded_json_is_parsed_once_and_native_json_is_preserved() {
        let schema = crate::schema::CollectionSchema::from_fields(
            &value!({"payload":{"type":"json"}, "label":{"type":"string"}}),
        )
        .unwrap()
        .into_fields();
        let values = value!([
            "true", "null", "42", "[1]", "{\"key\":1}", "\"nested\"", "plain text",
            true, false, 42, 1.5, null, {"key":"true"}, [false,"null"],
        ]);
        for value in values.as_array().unwrap() {
            for (registration, sqlite) in [
                (crate::sql::registration::SqlRegistration::postgres(), false),
                (crate::sql::registration::SqlRegistration::sqlite(), true),
            ] {
                let stored = if sqlite {
                    Value::String(serde_json::to_string(value).unwrap())
                } else {
                    value.clone()
                };
                let mut rows = [value!({"payload":stored, "label":"true"})];
                decode_rows(&registration, &schema, &mut rows).unwrap();
                assert_eq!(&rows[0]["payload"], value);
                assert_eq!(rows[0]["label"], Value::String("true".into()));

                rows[0]["payload"] = Value::Json(serde_json::to_string(value).unwrap());
                decode_rows(&registration, &schema, &mut rows).unwrap();
                assert_eq!(&rows[0]["payload"], value);
            }
        }
    }
}

#[cfg(test)]
mod binary_read_tests {
    use super::*;
    use crate::value;
    use crate::value::Value;

    fn schema() -> FieldMap {
        crate::schema::CollectionSchema::from_fields(&value!({"embedding":{"type":"vector", "vectorDims":2}, "location":{"type":"geoPoint"}})).unwrap().into_fields()
    }

    #[test]
    fn masked_storage_remains_opaque_to_logical_codecs() {
        for logical_type in [
            "boolean",
            "bytes",
            "vector",
            "geoPoint",
            "json",
            "date",
            "calendarDate",
        ] {
            let schema = crate::schema::CollectionSchema::from_fields(
                &value!({"classified":{"type":logical_type, "mask":{"kind":"full"}}}),
            )
            .unwrap()
            .into_fields();
            let mut row = value!({"classified":"***"});
            decode_rows(
                &crate::sql::registration::SqlRegistration::sqlite(),
                &schema,
                std::slice::from_mut(&mut row),
            )
            .unwrap();
            assert_eq!(row, value!({"classified":"***"}));
        }
        let unmasked = crate::schema::CollectionSchema::from_fields(
            &value!({"classified":{"type":"bytes", "mask":{"kind":"none"}}}),
        )
        .unwrap()
        .into_fields();
        let mut row = value!({"classified":"***"});
        assert!(decode_rows(
            &crate::sql::registration::SqlRegistration::sqlite(),
            &unmasked,
            std::slice::from_mut(&mut row)
        )
        .is_err());
    }

    #[test]
    fn binary_search_values_round_trip_as_logical_values() {
        let expected = value!({"embedding":[1.0, -0.5], "location":{"lat":37.0, "lng":-122.0}});
        let registration = crate::sql::registration::SqlRegistration::sqlite();
        let descriptor = schema();
        let mut stored = expected.clone();
        for field in ["embedding", "location"] {
            let storage = registration.storage_type(&descriptor[field]).unwrap();
            let value = std::mem::replace(&mut stored[field], Value::Null);
            stored[field] = registration.encode(storage, value).unwrap();
        }
        assert!(stored["embedding"].as_bytes().is_some());
        assert!(stored["location"].as_bytes().is_some());
        decode_rows(
            &registration,
            &descriptor,
            std::slice::from_mut(&mut stored),
        )
        .unwrap();
        assert_eq!(stored, expected);
        decode_rows(
            &registration,
            &descriptor,
            std::slice::from_mut(&mut stored),
        )
        .unwrap();
        assert_eq!(
            stored, expected,
            "normalization also accepts native PostgreSQL values"
        );
        let mut absent = value!({"embedding":null, "location":null});
        decode_rows(
            &registration,
            &descriptor,
            std::slice::from_mut(&mut absent),
        )
        .unwrap();
        assert_eq!(absent, value!({"embedding":null, "location":null}));
    }

    #[test]
    fn corrupt_binary_values_are_refused() {
        for (field, bytes) in [
            ("embedding", vec![]),
            ("embedding", vec![0]),
            (
                "embedding",
                crate::sql::sqlite_values::vec_to_le_bytes(&[1.0]),
            ),
            (
                "embedding",
                crate::sql::sqlite_values::vec_to_le_bytes(&[1.0, f32::INFINITY]),
            ),
            ("location", vec![0]),
            (
                "location",
                crate::sql::sqlite_values::point_to_blob(crate::sql::descriptors::GeoPoint {
                    lat: f64::NAN,
                    lng: 0.0,
                }),
            ),
            (
                "location",
                crate::sql::sqlite_values::point_to_blob(crate::sql::descriptors::GeoPoint {
                    lat: 91.0,
                    lng: 0.0,
                }),
            ),
        ] {
            let mut row = Value::Object([(field.to_owned(), Value::Bytes(bytes))].into());
            let error = decode_rows(
                &crate::sql::registration::SqlRegistration::sqlite(),
                &schema(),
                std::slice::from_mut(&mut row),
            )
            .unwrap_err();
            assert!(error.to_string().contains(field));
        }
        let registration = crate::sql::registration::SqlRegistration::sqlite();
        assert!(registration
            .encode(
                crate::sql::statement::StorageType::Vector,
                value!([f64::MAX, 0.0]),
            )
            .is_err());
    }
}
