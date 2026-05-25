use base64::Engine as _;
use serde_json::Value;

use crate::error::DbError;

pub(crate) enum SchemaFieldScope<'a> {
    All,
    Only(&'a [String]),
}

pub(crate) struct ApplyOptions<'a> {
    pub unmask_columns: &'a [String],
    pub schema_field_scope: SchemaFieldScope<'a>,
    pub apply_decrypt: bool,
    pub wrap_masked: bool,
}

impl<'a> Default for ApplyOptions<'a> {
    fn default() -> Self {
        Self {
            unmask_columns: &[],
            schema_field_scope: SchemaFieldScope::All,
            apply_decrypt: true,
            wrap_masked: true,
        }
    }
}

pub(crate) struct ApplyResult {
    pub rows: Vec<Value>,
    pub has_masked: bool,
}

/// Apply the canonical row-read pipeline once for every row-returning path.
///
/// The sequence is fixed:
///
/// 1. decode (already handled by the backend/exec layer before `rows` arrives)
/// 2. normalize
/// 3. decrypt encrypted columns
/// 4. wrap masked columns
/// 5. apply per-query unmask overrides
///
/// The cold-schema contract is intentional:
///
/// - schema-independent normalization still runs for the platform system
///   timestamps (`created_at`, `updated_at`, `deleted_at`)
/// - schema-driven coercions (`boolean`, `json`/`object`/`array`/`union`,
///   `bytes`, non-system `date`/`calendarDate`) only run when the per-isolate
///   schema cache is warm
/// - decrypt/mask/unmask metadata is schema-driven, so those stages only do
///   useful work when the schema cache is present
///
/// This keeps raw-JS / pre-register reads lossless instead of guessing at
/// ambiguous user data like `"{"ok":true}"` or `0/1` without a schema.
pub(crate) async fn apply(
    app_id: &str,
    collection: &str,
    mut rows: Vec<Value>,
    opts: ApplyOptions<'_>,
) -> Result<ApplyResult, DbError> {
    let schema = crate::context::with(|c| c.schema_for(app_id, collection))
        .map(|schema| scope_schema(schema, &opts.schema_field_scope));
    normalize_rows_on_read(schema.as_ref(), &mut rows)?;

    if opts.apply_decrypt {
        if let Some(schema) = schema.as_ref() {
            if super::schema_has_encrypted_columns(schema) {
                decrypt_rows_on_read(app_id, collection, schema, &mut rows).await?;
            }
        }
    }

    let has_masked = if opts.wrap_masked {
        if let Some(schema) = schema.as_ref() {
            if super::schema_has_masked_columns(schema) {
                wrap_masked_rows_on_read(collection, schema, &mut rows)?;
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    if !opts.unmask_columns.is_empty() {
        super::unmask::dispatch_unmask_for_query(app_id, collection, opts.unmask_columns, &mut rows)
            .await?;
    }

    Ok(ApplyResult { rows, has_masked })
}

fn scope_schema(mut schema: Value, scope: &SchemaFieldScope<'_>) -> Value {
    match scope {
        SchemaFieldScope::All => schema,
        SchemaFieldScope::Only(fields) => {
            let Some(obj) = schema.as_object_mut() else {
                return schema;
            };
            obj.retain(|key, _| key.starts_with('_') || fields.iter().any(|field| field == key));
            schema
        }
    }
}

fn normalize_rows_on_read(schema: Option<&Value>, rows: &mut [Value]) -> Result<(), DbError> {
    for row in rows.iter_mut() {
        normalize_row_on_read(schema, row)?;
    }
    Ok(())
}

fn normalize_row_on_read(schema: Option<&Value>, row: &mut Value) -> Result<(), DbError> {
    let Some(obj) = row.as_object_mut() else {
        return Ok(());
    };
    for (key, value) in obj.iter_mut() {
        if matches!(key.as_str(), "created_at" | "updated_at" | "deleted_at") {
            normalize_timestamp_value(value)?;
            continue;
        }

        let Some(def) = schema
            .and_then(Value::as_object)
            .and_then(|schema_obj| schema_obj.get(key))
            .and_then(Value::as_object)
        else {
            continue;
        };

        if def.get("encrypted").is_some() {
            continue;
        }

        match def.get("type").and_then(Value::as_str) {
            Some("boolean") => normalize_boolean_value(value)?,
            Some("json") | Some("object") | Some("array") | Some("union") => {
                normalize_json_value(value)
            }
            Some("bytes") => normalize_bytes_value(value)?,
            Some("date") | Some("calendarDate") => normalize_timestamp_value(value)?,
            _ => {}
        }
    }
    Ok(())
}

fn normalize_boolean_value(value: &mut Value) -> Result<(), DbError> {
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
                Err(DbError::internal(format!(
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
            other => Err(DbError::internal(format!(
                "normalize_row_on_read: boolean field expected 0/1/true/false, got {other:?}"
            ))),
        },
        other => Err(DbError::internal(format!(
            "normalize_row_on_read: boolean field expected bool/string/number/null, got {other:?}"
        ))),
    }
}

fn normalize_json_value(value: &mut Value) {
    if let Value::String(s) = value {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            *value = parsed;
        }
    }
}

fn normalize_bytes_value(value: &mut Value) -> Result<(), DbError> {
    match value {
        Value::Null | Value::String(_) => Ok(()),
        Value::Array(arr) => {
            let mut raw = Vec::with_capacity(arr.len());
            for cell in arr.iter() {
                let Some(n) = cell.as_u64() else {
                    return Err(DbError::internal(format!(
                        "normalize_row_on_read: bytes field expected byte array, got {cell:?}"
                    )));
                };
                let byte = u8::try_from(n).map_err(|_| {
                    DbError::internal(format!(
                        "normalize_row_on_read: bytes field byte out of range: {n}"
                    ))
                })?;
                raw.push(byte);
            }
            *value = Value::String(base64::engine::general_purpose::STANDARD.encode(raw));
            Ok(())
        }
        other => Err(DbError::internal(format!(
            "normalize_row_on_read: bytes field expected string/array/null, got {other:?}"
        ))),
    }
}

fn normalize_timestamp_value(value: &mut Value) -> Result<(), DbError> {
    match value {
        Value::Null | Value::Number(_) => Ok(()),
        Value::String(s) => {
            if let Some(ms) = parse_timestamp_millis(s) {
                *value = Value::Number(serde_json::Number::from(ms));
            }
            Ok(())
        }
        other => Err(DbError::internal(format!(
            "normalize_row_on_read: timestamp field expected string/number/null, got {other:?}"
        ))),
    }
}

fn parse_timestamp_millis(s: &str) -> Option<i64> {
    if let Some(ms) = crate::backend::sqlite::session_minter::parse_iso_to_millis(s) {
        return Some(ms);
    }

    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b' ' | b'T') || b[13] != b':' || b[16] != b':' {
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
    let y = if m <= 2 { i64::from(y) - 1 } else { i64::from(y) };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let m = m as i64;
    let d = d as i64;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as u64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

async fn decrypt_rows_on_read(
    app_id: &str,
    collection: &str,
    schema: &Value,
    rows: &mut [Value],
) -> Result<(), DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;
    if let Some(pg) = backend.as_encrypted_column_pg() {
        for row in rows.iter_mut() {
            crate::crud::encryption_pass::decrypt_row_on_read(pg, app_id, collection, schema, row)
                .await?;
        }
        return Ok(());
    }
    if let Some(sq) = backend.as_encrypted_column_sqlite() {
        for row in rows.iter_mut() {
            crate::crud::encryption_pass::decrypt_row_on_read(sq, app_id, collection, schema, row)
                .await?;
        }
    }
    Ok(())
}

fn wrap_masked_rows_on_read(
    collection: &str,
    schema: &Value,
    rows: &mut [Value],
) -> Result<(), DbError> {
    for row in rows.iter_mut() {
        crate::crud::mask_pass::wrap_row_on_read(schema, collection, row)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_row_on_read_coerces_sqlite_wire_shapes() {
        let schema = serde_json::json!({
            "active": { "type": "boolean" },
            "prefs": { "type": "object" },
            "avatar": { "type": "bytes" },
            "published_at": { "type": "date" }
        });
        let mut row = serde_json::json!({
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}",
            "avatar": [104, 105],
            "published_at": "2026-05-07T01:02:03.004Z"
        });

        normalize_row_on_read(Some(&schema), &mut row).expect("normalize");

        assert_eq!(row["active"], Value::Bool(true));
        assert_eq!(row["prefs"], serde_json::json!({"theme":"dark"}));
        assert_eq!(row["avatar"], Value::String("aGk=".to_string()));
        assert_eq!(row["published_at"], serde_json::json!(1_778_115_723_004i64));
    }

    #[test]
    fn normalize_row_on_read_skips_encrypted_columns() {
        let schema = serde_json::json!({
            "secret": {
                "type": "bytes",
                "encrypted": {
                    "mode": "randomized",
                    "wraps": "bytes"
                }
            }
        });
        let mut row = serde_json::json!({
            "secret": "AQID"
        });

        normalize_row_on_read(Some(&schema), &mut row).expect("normalize");

        assert_eq!(row["secret"], Value::String("AQID".to_string()));
    }

    #[test]
    fn normalize_row_on_read_without_schema_only_normalizes_system_timestamps() {
        let mut row = serde_json::json!({
            "created_at": "2026-05-07T01:02:03.004Z",
            "published_at": "2026-05-07T01:02:03.004Z",
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}"
        });

        normalize_row_on_read(None, &mut row).expect("normalize");

        assert_eq!(row["created_at"], serde_json::json!(1_778_115_723_004i64));
        assert_eq!(row["published_at"], Value::String("2026-05-07T01:02:03.004Z".to_string()));
        assert_eq!(row["active"], serde_json::json!(1));
        assert_eq!(row["prefs"], Value::String("{\"theme\":\"dark\"}".to_string()));
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
        let schema = serde_json::json!({
            "active": { "type": "boolean" }
        });
        let mut row = serde_json::json!({
            "active": 2
        });

        let err = normalize_row_on_read(Some(&schema), &mut row)
            .expect_err("declared boolean field must reject out-of-domain values");
        match err {
            DbError::Internal { message } => {
                assert!(
                    message.contains("boolean field expected 0/1"),
                    "error should explain the boolean domain violation: {message}"
                );
            }
            other => panic!("expected Internal error, got {other:?}"),
        }
    }

    #[test]
    fn scoped_schema_excludes_aggregate_alias_collisions() {
        crate::context::with_mut(|c| {
            c.cache_schema(
                "app_aggregate_scope",
                "users",
                serde_json::json!({
                    "secret": {
                        "type": "string",
                        "encrypted": {
                            "mode": "randomised",
                            "keyId": "default",
                            "wraps": "string"
                        },
                        "mask": {
                            "kind": "last4",
                            "classification": "spi"
                        }
                    }
                }),
            );
        });

        let rows = vec![serde_json::json!({
            "secret": 3
        })];

        let result = compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(apply(
                "app_aggregate_scope",
                "users",
                rows,
                ApplyOptions {
                    unmask_columns: &[],
                    schema_field_scope: SchemaFieldScope::Only(&[]),
                    ..ApplyOptions::default()
                },
            ))
            .expect("aggregate aliases must bypass schema-driven transforms");

        assert_eq!(result.rows, vec![serde_json::json!({ "secret": 3 })]);
        assert!(!result.has_masked);
    }

    #[test]
    fn apply_can_skip_mask_wrapping_for_distinct_scalars() {
        crate::context::with_mut(|c| {
            c.cache_schema(
                "app_distinct_masked",
                "users",
                serde_json::json!({
                    "email": {
                        "type": "string",
                        "mask": { "kind": "email", "classification": "pii" }
                    }
                }),
            );
        });

        let rows = vec![serde_json::json!({
            "email": "a***@example.com"
        })];

        let result = compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(apply(
                "app_distinct_masked",
                "users",
                rows,
                ApplyOptions {
                    wrap_masked: false,
                    ..ApplyOptions::default()
                },
            ))
            .expect("distinct scalars should bypass masked-value wrapping");

        assert_eq!(
            result.rows,
            vec![serde_json::json!({ "email": "a***@example.com" })]
        );
        assert!(!result.has_masked);
    }
}
