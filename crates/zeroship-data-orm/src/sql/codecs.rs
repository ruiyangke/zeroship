//! Logical values and their database storage representations.
use crate::sql::{compile, descriptors::GeoPoint};
use crate::value::Value;

mod typed;
pub(crate) use typed::{prepare_array_operand, prepare_value};
pub use typed::{prepare_document, prepare_update};

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

fn schema_field<'a>(schema: &'a Value, field: &str) -> Option<&'a Value> {
    schema.as_object()?.get(field)
}

fn encode_sqlite_binary_scalar(
    field: &str,
    field_def: &Value,
    value: &mut Value,
) -> Result<(), CodecError> {
    if value.is_null() {
        return Ok(());
    }

    match field_def.get("type").and_then(Value::as_str) {
        Some("vector") => {
            let dims = field_def
                .get("vectorDims")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    CodecError::internal(format!(
                        "sqlite vector write encoding: schema for '{field}' is missing vectorDims"
                    ))
                })? as usize;
            let arr = value.as_array().ok_or_else(|| {
                CodecError::validation(
                    "invalid_vector_arg",
                    format!("db: vector column '{field}' must be a number[]"),
                )
            })?;
            if arr.len() != dims {
                return Err(CodecError::validation(
                    "vector_dimension_mismatch",
                    format!(
                        "db: vector column '{field}' expected {dims} dimensions, got {}",
                        arr.len()
                    ),
                ));
            }
            let mut vector = Vec::with_capacity(dims);
            for item in arr {
                let n = item.as_f64().ok_or_else(|| {
                    CodecError::validation(
                        "invalid_vector_arg",
                        format!("db: vector column '{field}' must contain only numbers"),
                    )
                })?;
                let component = n as f32;
                if !component.is_finite() {
                    return Err(CodecError::validation(
                        "invalid_vector_arg",
                        format!("db: vector column '{field}' contains an out-of-range component"),
                    ));
                }
                vector.push(component);
            }
            *value = Value::Bytes(crate::sql::sqlite_values::vec_to_le_bytes(&vector));
            Ok(())
        }
        Some("geoPoint") => {
            let obj = value.as_object().ok_or_else(|| {
                CodecError::validation(
                    "invalid_geo_arg",
                    format!("db: geoPoint column '{field}' must be an object with lat/lng"),
                )
            })?;
            let lat = obj.get("lat").and_then(Value::as_f64).ok_or_else(|| {
                CodecError::validation(
                    "invalid_geo_arg",
                    format!("db: geoPoint column '{field}' is missing numeric lat"),
                )
            })?;
            let lng = obj.get("lng").and_then(Value::as_f64).ok_or_else(|| {
                CodecError::validation(
                    "invalid_geo_arg",
                    format!("db: geoPoint column '{field}' is missing numeric lng"),
                )
            })?;
            if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
                return Err(CodecError::validation(
                    "invalid_geo_arg",
                    format!("db: geoPoint column '{field}' contains out-of-range coordinates"),
                ));
            }
            *value = Value::Bytes(crate::sql::sqlite_values::point_to_blob(GeoPoint {
                lat,
                lng,
            }));
            Ok(())
        }
        _ => Ok(()),
    }
}

fn encode_sqlite_binary_doc_with_schema(schema: &Value, doc: &mut Value) -> Result<(), CodecError> {
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    let Some(schema_obj) = schema.as_object() else {
        return Ok(());
    };
    for (field, field_def) in schema_obj {
        let Some(value) = obj.get_mut(field) else {
            continue;
        };
        encode_sqlite_binary_scalar(field, field_def, value)?;
    }
    Ok(())
}

fn encode_sqlite_binary_update_with_schema(
    schema: &Value,
    patch: &mut Value,
) -> Result<(), CodecError> {
    let Some(obj) = patch.as_object_mut() else {
        return Ok(());
    };
    if let Some(set_doc) = obj.get_mut("$set") {
        encode_sqlite_binary_doc_with_schema(schema, set_doc)?;
    }
    for (field, value) in obj.iter_mut() {
        if field.starts_with('$') {
            continue;
        }
        let Some(field_def) = schema_field(schema, field) else {
            continue;
        };
        match value {
            Value::Array(_) | Value::String(_) | Value::Object(_) | Value::Null => {
                if let Some(set_val) = value.as_object_mut().and_then(|ops| ops.get_mut("$set")) {
                    encode_sqlite_binary_scalar(field, field_def, set_val)?;
                } else {
                    encode_sqlite_binary_scalar(field, field_def, value)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn has_storage_encoding(schema: &Value) -> bool {
    schema_has_sqlite_binary_columns(schema)
}
pub fn encode_document(
    dialect: compile::SqlDialect,
    schema: &Value,
    value: &mut Value,
) -> Result<(), CodecError> {
    if dialect == compile::SqlDialect::Sqlite {
        encode_sqlite_binary_doc_with_schema(schema, value)?;
    }
    Ok(())
}
pub fn encode_update(
    dialect: compile::SqlDialect,
    schema: &Value,
    value: &mut Value,
) -> Result<(), CodecError> {
    if dialect == compile::SqlDialect::Sqlite {
        encode_sqlite_binary_update_with_schema(schema, value)?;
    }
    Ok(())
}
fn schema_has_sqlite_binary_columns(schema: &Value) -> bool {
    schema
        .as_object()
        .map(|o| {
            o.values().any(|def| {
                matches!(
                    def.get("type").and_then(Value::as_str),
                    Some("vector") | Some("geoPoint")
                )
            })
        })
        .unwrap_or(false)
}
/// Convert driver results to logical values. SQLite JSON text is still encoded;
/// PostgreSQL JSON values have already been decoded by the wire protocol codec.
pub fn decode_rows(
    dialect: compile::SqlDialect,
    schema: &Value,
    rows: &mut [Value],
) -> Result<(), CodecError> {
    for row in rows.iter_mut() {
        normalize_row_on_read(dialect, schema, row)?;
    }
    Ok(())
}

fn normalize_row_on_read(
    dialect: compile::SqlDialect,
    schema: &Value,
    row: &mut Value,
) -> Result<(), CodecError> {
    let Some(obj) = row.as_object_mut() else {
        return Ok(());
    };
    for (key, value) in obj.iter_mut() {
        let Some(def) = schema
            .as_object()
            .and_then(|schema_obj| schema_obj.get(key))
            .and_then(Value::as_object)
        else {
            continue;
        };

        // Protected storage is decoded by the protection pipeline. A mask is
        // stored as text even when its logical field is numeric or binary.
        if def.get("encrypted").and_then(Value::as_bool) == Some(true)
            || compile::column_is_masked(key, schema)
        {
            continue;
        }

        match def.get("type").and_then(Value::as_str) {
            Some("boolean") => normalize_boolean_value(value)?,
            Some("json") | Some("object") | Some("array") | Some("union") => {
                normalize_json_value(dialect, key, value)?;
                prepare_value(key, &schema[key], value).map_err(|_| CodecError::Decode {
                    column: key.clone(),
                    reason: "invalid typed JSON storage",
                })?;
            }
            Some("bytes") => normalize_bytes_value(value)?,
            Some("date" | "timestamp") => normalize_timestamp_value(key, value)?,
            Some("calendarDate") => {
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
            Some("vector") => normalize_vector_value(key, def, value)?,
            Some("geoPoint") => normalize_geopoint_value(key, value)?,
            _ => {}
        }
    }
    Ok(())
}

fn normalize_vector_value(
    field: &str,
    definition: &crate::value::Map<String, Value>,
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
            let dimensions = definition
                .get("vectorDims")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok());
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

fn normalize_json_value(
    dialect: compile::SqlDialect,
    field: &str,
    value: &mut Value,
) -> Result<(), CodecError> {
    let encoded = match value {
        Value::Json(encoded) => encoded,
        Value::String(encoded) if dialect == compile::SqlDialect::Sqlite => encoded,
        _ => return Ok(()),
    };
    *value = serde_json::from_str(encoded).map_err(|_| CodecError::Decode {
        column: field.to_owned(),
        reason: "invalid JSON storage",
    })?;
    Ok(())
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
    fn encode_sqlite_binary_doc_with_schema_packs_vector_and_geopoint() {
        let schema = crate::value!({
            "embedding": { "type": "vector", "vectorDims": 4 },
            "loc": { "type": "geoPoint" },
            "name": { "type": "string" }
        });
        let mut doc = crate::value!({
            "embedding": [1.0, 0.0, 0.5, -1.25],
            "loc": { "lat": 37.7749, "lng": -122.4194 },
            "name": "Alpha HQ"
        });

        encode_sqlite_binary_doc_with_schema(&schema, &mut doc).expect("encode sqlite blobs");

        let embedding_bytes = doc["embedding"].as_bytes().expect("native vector buffer");
        let loc_bytes = doc["loc"].as_bytes().expect("native geography buffer");
        assert!(doc.get("__zsbin__embedding").is_none());
        assert!(doc.get("__zsbin__loc").is_none());

        assert_eq!(
            embedding_bytes,
            crate::sql::sqlite_values::vec_to_le_bytes(&[1.0, 0.0, 0.5, -1.25]),
        );
        assert_eq!(
            loc_bytes,
            crate::sql::sqlite_values::point_to_blob(GeoPoint {
                lat: 37.7749,
                lng: -122.4194,
            }),
        );
    }
    #[test]
    fn normalize_row_on_read_coerces_sqlite_wire_shapes() {
        let schema = crate::value!({
            "active": { "type": "boolean" },
            "prefs": { "type": "object" },
            "avatar": { "type": "bytes" },
            "published_at": { "type": "date" }
        });
        let mut row = crate::value!({
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}",
            "avatar": Value::Bytes(vec![104, 105]),
            "published_at": "2026-05-07T01:02:03.004Z"
        });

        normalize_row_on_read(compile::SqlDialect::Sqlite, &schema, &mut row).expect("normalize");

        assert_eq!(row["active"], Value::Bool(true));
        assert_eq!(row["prefs"], crate::value!({"theme":"dark"}));
        assert_eq!(row["avatar"], Value::Bytes(vec![104, 105]));
        assert_eq!(row["published_at"], Value::Timestamp(1_778_115_723_004));
    }
    #[test]
    fn normalize_row_on_read_skips_encrypted_columns() {
        let schema = crate::value!({
            "secret": {
                "type": "bytes",
                "encrypted": true
            }
        });
        let mut row = crate::value!({
            "secret": "AQID"
        });

        normalize_row_on_read(compile::SqlDialect::Sqlite, &schema, &mut row).expect("normalize");

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
            compile::SqlDialect::Sqlite,
            &crate::sql::compile::empty_read_schema(),
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
        let schema = crate::value!({
            "created_at":{"type":"string"},
            "updated_at":{"type":"int"},
            "occurred_at":{"type":"date"}
        });
        let mut rows = vec![
            crate::value!({"created_at":"ordinary text", "updated_at":9, "occurred_at":"2026-05-07T01:02:03.004Z"}),
        ];
        decode_rows(compile::SqlDialect::Sqlite, &schema, &mut rows).unwrap();
        assert_eq!(rows[0]["created_at"], crate::value!("ordinary text"));
        assert_eq!(rows[0]["updated_at"], crate::value!(9));
        assert_eq!(rows[0]["occurred_at"], Value::Timestamp(1_778_115_723_004));
        let mut parameters = Vec::new();
        let sql = compile::build_where_with_dialect(
            &crate::value!({"created_at":"ordinary text"}),
            &mut parameters,
            &schema,
            compile::SqlDialect::Postgres,
        )
        .unwrap();
        assert!(!sql.contains("timestamptz"));
        assert_eq!(parameters, vec![crate::value!("ordinary text")]);
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
        let schema = crate::value!({
            "active": { "type": "boolean" }
        });
        let mut row = crate::value!({
            "active": 2
        });

        let err = normalize_row_on_read(compile::SqlDialect::Sqlite, &schema, &mut row)
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
            let schema = value!({"instant":{"type":kind}});
            for dialect in [compile::SqlDialect::Postgres, compile::SqlDialect::Sqlite] {
                for value in [
                    value!(-1),
                    value!(-1.0),
                    Value::Timestamp(-1),
                    value!("1969-12-31T23:59:59.999999Z"),
                ] {
                    let mut rows = [value!({"instant":value})];
                    decode_rows(dialect, &schema, &mut rows).unwrap();
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
                    let error = decode_rows(dialect, &schema, &mut rows).unwrap_err();
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
        let schema = value!({
            "masked":{"type":"date", "mask":{"kind":"full"}},
            "encrypted":{"type":"string", "encrypted":true},
        });
        let original = value!({"masked":"***", "encrypted":Value::Bytes(vec![1, 2, 3])});
        let mut rows = [original.clone()];
        decode_rows(compile::SqlDialect::Sqlite, &schema, &mut rows).unwrap();
        assert_eq!(rows[0], original);
    }
}

#[cfg(test)]
mod calendar_date_tests {
    use super::*;
    use crate::value;

    #[test]
    fn calendar_date_reads_reject_invalid_storage_without_echoing_it() {
        let schema = value!({"birthday":{"type":"calendarDate"}});
        for stored in [
            value!("2026-02-30"),
            value!("1900-02-29"),
            value!("0000-01-01"),
            value!("2026-01-01T00:00:00Z"),
            value!("private_not_a_date"),
            value!(0),
        ] {
            let mut rows = [value!({"birthday":stored})];
            let error = decode_rows(compile::SqlDialect::Sqlite, &schema, &mut rows).unwrap_err();
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
        let schema = value!({"payload":{"type":"json"}});
        for text in ["true", "null", "42", "[1]", "{\"key\":1}", "\"nested\""] {
            let mut rows = [value!({"payload":text})];
            decode_rows(compile::SqlDialect::Postgres, &schema, &mut rows).unwrap();
            assert_eq!(rows[0]["payload"], Value::String(text.into()));
        }
    }

    #[test]
    fn sqlite_json_text_requires_valid_json() {
        let schema = value!({"payload":{"type":"json"}});
        let mut rows = [value!({"payload":"secret_invalid_json"})];
        let error = decode_rows(compile::SqlDialect::Sqlite, &schema, &mut rows).unwrap_err();
        assert!(error.to_string().contains("payload"));
        assert!(!error.to_string().contains("secret_invalid_json"));
    }

    #[test]
    fn encoded_json_is_parsed_once_and_native_json_is_preserved() {
        let schema = value!({"payload":{"type":"json"}, "label":{"type":"string"}});
        let values = value!([
            "true", "null", "42", "[1]", "{\"key\":1}", "\"nested\"", "plain text",
            true, false, 42, 1.5, null, {"key":"true"}, [false,"null"],
        ]);
        for value in values.as_array().unwrap() {
            for dialect in [compile::SqlDialect::Postgres, compile::SqlDialect::Sqlite] {
                let stored = if dialect == compile::SqlDialect::Sqlite {
                    Value::String(serde_json::to_string(value).unwrap())
                } else {
                    value.clone()
                };
                let mut rows = [value!({"payload":stored, "label":"true"})];
                decode_rows(dialect, &schema, &mut rows).unwrap();
                assert_eq!(&rows[0]["payload"], value);
                assert_eq!(rows[0]["label"], Value::String("true".into()));

                rows[0]["payload"] = Value::Json(serde_json::to_string(value).unwrap());
                decode_rows(dialect, &schema, &mut rows).unwrap();
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

    fn schema() -> Value {
        value!({"embedding":{"type":"vector", "vectorDims":2}, "location":{"type":"geoPoint"}})
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
            let schema = value!({"classified":{"type":logical_type, "mask":{"kind":"full"}}});
            let mut row = value!({"classified":"***"});
            decode_rows(
                compile::SqlDialect::Sqlite,
                &schema,
                std::slice::from_mut(&mut row),
            )
            .unwrap();
            assert_eq!(row, value!({"classified":"***"}));
        }
        let unmasked = value!({"classified":{"type":"bytes", "mask":{"kind":"none"}}});
        let mut row = value!({"classified":"***"});
        assert!(decode_rows(
            compile::SqlDialect::Sqlite,
            &unmasked,
            std::slice::from_mut(&mut row)
        )
        .is_err());
    }

    #[test]
    fn binary_search_values_round_trip_as_logical_values() {
        let expected = value!({"embedding":[1.0, -0.5], "location":{"lat":37.0, "lng":-122.0}});
        let mut stored = expected.clone();
        encode_document(compile::SqlDialect::Sqlite, &schema(), &mut stored).unwrap();
        assert!(stored["embedding"].as_bytes().is_some());
        assert!(stored["location"].as_bytes().is_some());
        decode_rows(
            compile::SqlDialect::Sqlite,
            &schema(),
            std::slice::from_mut(&mut stored),
        )
        .unwrap();
        assert_eq!(stored, expected);
        decode_rows(
            compile::SqlDialect::Sqlite,
            &schema(),
            std::slice::from_mut(&mut stored),
        )
        .unwrap();
        assert_eq!(
            stored, expected,
            "normalization also accepts native PostgreSQL values"
        );
        let mut absent = value!({"embedding":null, "location":null});
        decode_rows(
            compile::SqlDialect::Sqlite,
            &schema(),
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
                crate::sql::sqlite_values::point_to_blob(GeoPoint {
                    lat: f64::NAN,
                    lng: 0.0,
                }),
            ),
            (
                "location",
                crate::sql::sqlite_values::point_to_blob(GeoPoint {
                    lat: 91.0,
                    lng: 0.0,
                }),
            ),
        ] {
            let mut row = Value::Object([(field.to_owned(), Value::Bytes(bytes))].into());
            let error = decode_rows(
                compile::SqlDialect::Sqlite,
                &schema(),
                std::slice::from_mut(&mut row),
            )
            .unwrap_err();
            assert!(error.to_string().contains(field));
        }
        let mut row = value!({"embedding":[f64::MAX, 0.0]});
        assert!(encode_document(compile::SqlDialect::Sqlite, &schema(), &mut row).is_err());
    }
}
