//! Logical values and their database storage representations.
use crate::{compile, descriptors::GeoPoint, value::Value};

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
    fn validation(code: &'static str, message: impl Into<String>) -> Self {
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

pub fn lower_document(dialect: compile::SqlDialect, schema: &Value, doc: &mut Value) {
    if dialect != compile::SqlDialect::Sqlite {
        return;
    }
    lower_boolean_doc_with_schema(schema, doc);
}

pub fn lower_documents(dialect: compile::SqlDialect, schema: &Value, docs: &mut Value) {
    if dialect != compile::SqlDialect::Sqlite {
        return;
    }
    let Some(arr) = docs.as_array_mut() else {
        return;
    };
    for doc in arr {
        lower_boolean_doc_with_schema(schema, doc);
    }
}

pub fn lower_update(dialect: compile::SqlDialect, schema: &Value, patch: &mut Value) {
    if dialect != compile::SqlDialect::Sqlite {
        return;
    }
    lower_boolean_update_with_schema(schema, patch);
}

pub fn lower_filter(dialect: compile::SqlDialect, schema: &Value, filter: &mut Value) {
    if dialect != compile::SqlDialect::Sqlite {
        return;
    }
    lower_boolean_filter_with_schema(schema, filter);
}

fn lower_boolean_doc_with_schema(schema: &Value, doc: &mut Value) {
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    for (field, value) in obj.iter_mut() {
        if schema_field_type(schema, field) == Some("boolean") {
            lower_boolean_scalar(value);
        }
    }
}

fn lower_boolean_update_with_schema(schema: &Value, patch: &mut Value) {
    let Some(obj) = patch.as_object_mut() else {
        return;
    };
    if let Some(set_doc) = obj.get_mut("$set") {
        lower_boolean_doc_with_schema(schema, set_doc);
    }
    for (field, value) in obj {
        if field.starts_with('$') {
            continue;
        }
        if schema_field_type(schema, field) != Some("boolean") {
            continue;
        }
        match value {
            Value::Bool(_) => lower_boolean_scalar(value),
            Value::Object(ops) => {
                if let Some(set_val) = ops.get_mut("$set") {
                    lower_boolean_scalar(set_val);
                }
            }
            _ => {}
        }
    }
}

fn lower_boolean_filter_with_schema(schema: &Value, filter: &mut Value) {
    let Some(obj) = filter.as_object_mut() else {
        return;
    };
    for (key, value) in obj {
        if key.starts_with('$') {
            match key.as_str() {
                "$and" | "$or" => {
                    if let Some(arr) = value.as_array_mut() {
                        for clause in arr {
                            lower_boolean_filter_with_schema(schema, clause);
                        }
                    }
                }
                "$not" => lower_boolean_filter_with_schema(schema, value),
                _ => {}
            }
            continue;
        }
        if schema_field_type(schema, key) == Some("boolean") {
            lower_boolean_filter_value(value);
        }
    }
}

fn lower_boolean_filter_value(value: &mut Value) {
    match value {
        Value::Bool(_) => lower_boolean_scalar(value),
        Value::Object(ops) => {
            for (op, operand) in ops {
                match op.as_str() {
                    "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte" => {
                        lower_boolean_scalar(operand);
                    }
                    "$in" | "$nin" => {
                        if let Some(arr) = operand.as_array_mut() {
                            for item in arr {
                                lower_boolean_scalar(item);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn lower_boolean_scalar(value: &mut Value) {
    if let Value::Bool(b) = value {
        *value = Value::Number(crate::value::Number::from(i64::from(u8::from(*b))));
    }
}

fn schema_field_type<'a>(schema: &'a Value, field: &str) -> Option<&'a str> {
    schema.as_object()?.get(field)?.get("type")?.as_str()
}

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
            *value = Value::Bytes(crate::sqlite_values::vec_to_le_bytes(&vector));
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
            *value = Value::Bytes(crate::sqlite_values::point_to_blob(GeoPoint { lat, lng }));
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
        if matches!(key.as_str(), "created_at" | "updated_at" | "deleted_at") {
            normalize_timestamp_value(value)?;
            continue;
        }

        let Some(def) = schema
            .as_object()
            .and_then(|schema_obj| schema_obj.get(key))
            .and_then(Value::as_object)
        else {
            continue;
        };

        // Protected storage is decoded by the protection pipeline. A mask is
        // stored as text even when its logical field is numeric or binary.
        if def.get("encrypted").is_some() || compile::column_is_masked(key, schema) {
            continue;
        }

        match def.get("type").and_then(Value::as_str) {
            Some("boolean") => normalize_boolean_value(value)?,
            Some("json") | Some("object") | Some("array") | Some("union") => {
                normalize_json_value(dialect, key, value)?;
            }
            Some("bytes") => normalize_bytes_value(value)?,
            Some("date") | Some("calendarDate") => normalize_timestamp_value(value)?,
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

fn normalize_timestamp_value(value: &mut Value) -> Result<(), CodecError> {
    match value {
        Value::Null | Value::Number(_) | Value::Timestamp(_) => Ok(()),
        Value::String(s) => {
            if let Some(ms) = parse_timestamp_millis(s) {
                *value = Value::Number(crate::value::Number::from(ms));
            }
            Ok(())
        }
        other => Err(CodecError::internal(format!(
            "normalize_row_on_read: timestamp field expected string/number/null, got {other:?}"
        ))),
    }
}

// This used to try `session_minter::parse_iso_to_millis` first and fall back to
// the parser below. That fast path was deleted with the session minter on
// 2026-09-02, and nothing was lost: the parser below accepts a strict superset
// of the same `YYYY-MM-DDTHH:MM:SS.mmm` shape and computes it identically. It
// is also stricter where it matters - the deleted one range-checked no field,
// so `...T99:00:00.000` short-circuited to a nonsense instant instead of `None`.
fn parse_timestamp_millis(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b' ' | b'T')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }

    let year: i32 = std::str::from_utf8(&b[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&b[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&b[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&b[11..13]).ok()?.parse().ok()?;
    let minute: i64 = std::str::from_utf8(&b[14..16]).ok()?.parse().ok()?;
    let second: i64 = std::str::from_utf8(&b[17..19]).ok()?.parse().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return None;
    }

    let mut millis = 0i64;
    let mut idx = 19usize;

    if idx < b.len() && b[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == frac_start {
            return None;
        }
        let frac = &b[frac_start..idx];
        let frac_digits = std::str::from_utf8(frac).ok()?;
        let mut milli_digits = frac_digits.chars().take(3).collect::<String>();
        while milli_digits.len() < 3 {
            milli_digits.push('0');
        }
        millis = milli_digits.parse::<i64>().ok()?;
    }

    let tz_offset_minutes = if idx < b.len() {
        parse_timestamp_offset_minutes(&b[idx..])?
    } else {
        0
    };

    let days = days_from_civil(year, month, day)?;
    let total_secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    Some(total_secs * 1000 + millis - tz_offset_minutes * 60 * 1000)
}

fn parse_timestamp_offset_minutes(rest: &[u8]) -> Option<i64> {
    match rest {
        b"Z" | b"z" => Some(0),
        [sign @ (b'+' | b'-'), h1, h2] => {
            let hours: i64 = std::str::from_utf8(&[*h1, *h2]).ok()?.parse().ok()?;
            if hours > 23 {
                return None;
            }
            let sign = if *sign == b'-' { -1 } else { 1 };
            Some(sign * hours * 60)
        }
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let hours: i64 = std::str::from_utf8(&[*h1, *h2]).ok()?.parse().ok()?;
            let minutes: i64 = std::str::from_utf8(&[*m1, *m2]).ok()?.parse().ok()?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let sign = if *sign == b'-' { -1 } else { 1 };
            Some(sign * (hours * 60 + minutes))
        }
        [sign @ (b'+' | b'-'), h1, h2, m1, m2] => {
            let hours: i64 = std::str::from_utf8(&[*h1, *h2]).ok()?.parse().ok()?;
            let minutes: i64 = std::str::from_utf8(&[*m1, *m2]).ok()?.parse().ok()?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let sign = if *sign == b'-' { -1 } else { 1 };
            Some(sign * (hours * 60 + minutes))
        }
        _ => None,
    }
}

fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 {
        i64::from(y) - 1
    } else {
        i64::from(y)
    };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let m = m as i64;
    let d = d as i64;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as u64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

/// Decrypt every encrypted column on `rows`.
///
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lower_boolean_filter_with_schema_keeps_json_booleans_untouched() {
        let schema = crate::value!({
            "active": { "type": "boolean" },
            "payload": { "type": "json" }
        });
        let mut filter = crate::value!({
            "$and": [
                { "active": { "$in": [true, false] } },
                { "payload": true }
            ]
        });

        lower_boolean_filter_with_schema(&schema, &mut filter);

        assert_eq!(filter["$and"][0]["active"]["$in"], crate::value!([1, 0]));
        assert_eq!(filter["$and"][1]["payload"], Value::Bool(true));
    }
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
            crate::sqlite_values::vec_to_le_bytes(&[1.0, 0.0, 0.5, -1.25]),
        );
        assert_eq!(
            loc_bytes,
            crate::sqlite_values::point_to_blob(GeoPoint {
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
        assert_eq!(row["published_at"], crate::value!(1_778_115_723_004i64));
    }
    #[test]
    fn normalize_row_on_read_skips_encrypted_columns() {
        let schema = crate::value!({
            "secret": {
                "type": "bytes",
                "encrypted": {
                    "mode": "randomized",
                    "wraps": "bytes"
                }
            }
        });
        let mut row = crate::value!({
            "secret": "AQID"
        });

        normalize_row_on_read(compile::SqlDialect::Sqlite, &schema, &mut row).expect("normalize");

        assert_eq!(row["secret"], Value::String("AQID".to_string()));
    }
    #[test]
    fn normalize_row_on_read_without_declared_fields_only_normalizes_system_timestamps() {
        let mut row = crate::value!({
            "created_at": "2026-05-07T01:02:03.004Z",
            "published_at": "2026-05-07T01:02:03.004Z",
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}"
        });

        normalize_row_on_read(
            compile::SqlDialect::Sqlite,
            &crate::compile::empty_read_schema(),
            &mut row,
        )
        .expect("normalize");

        assert_eq!(row["created_at"], crate::value!(1_778_115_723_004i64));
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
    fn parse_timestamp_millis_accepts_iso_z_and_variable_fraction() {
        let expected = 1_746_579_723_004i64;
        assert_eq!(
            parse_timestamp_millis("2025-05-07T01:02:03.004Z"),
            Some(expected)
        );
        assert_eq!(
            parse_timestamp_millis("2025-05-07T01:02:03.004999Z"),
            Some(expected)
        );
        assert_eq!(
            parse_timestamp_millis("2025-05-07T03:02:03.004+02:00"),
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
    use crate::{value, value::Value};

    fn schema() -> Value {
        value!({"embedding":{"type":"vector", "vectorDims":2}, "location":{"type":"geoPoint"}})
    }

    #[test]
    fn masked_storage_remains_opaque_to_logical_codecs() {
        for logical_type in ["boolean", "bytes", "vector", "geoPoint", "json", "date"] {
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
        assert!(
            decode_rows(
                compile::SqlDialect::Sqlite,
                &unmasked,
                std::slice::from_mut(&mut row)
            )
            .is_err()
        );
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
            ("embedding", crate::sqlite_values::vec_to_le_bytes(&[1.0])),
            (
                "embedding",
                crate::sqlite_values::vec_to_le_bytes(&[1.0, f32::INFINITY]),
            ),
            ("location", vec![0]),
            (
                "location",
                crate::sqlite_values::point_to_blob(GeoPoint {
                    lat: f64::NAN,
                    lng: 0.0,
                }),
            ),
            (
                "location",
                crate::sqlite_values::point_to_blob(GeoPoint {
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
