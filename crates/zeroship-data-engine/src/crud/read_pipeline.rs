use std::sync::Arc;

use zeroship_data_query_builder::value::Value;

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;

pub enum SchemaFieldScope<'a> {
    All,
    Only(&'a [String]),
}

/// Which key names a decoded row may carry across the JS boundary.
///
/// **There is no unrestricted arm**, and [`RowSurface::Declared`] is the
/// `Default`, so a call site that says nothing gets the safe answer and a call
/// site that wants something else has to name a list. That is the whole
/// defence against the next row-returning verb: it cannot opt out of the
/// surface filter because there is nothing to opt out to.
pub enum RowSurface<'a> {
    /// Declared fields + the seven system fields + the closed set of synthetic
    /// result columns. The default.
    Declared,
    /// An explicit name list. Aggregate result sets only: their keys are
    /// accumulator aliases, which no descriptor declares.
    Projected(&'a [String]),
}

pub struct ApplyOptions<'a> {
    pub unmask_columns: &'a [String],
    pub schema_field_scope: SchemaFieldScope<'a>,
    pub apply_decrypt: bool,
    pub wrap_masked: bool,
    pub row_surface: RowSurface<'a>,
}

impl<'a> Default for ApplyOptions<'a> {
    fn default() -> Self {
        Self {
            unmask_columns: &[],
            schema_field_scope: SchemaFieldScope::All,
            apply_decrypt: true,
            wrap_masked: true,
            row_surface: RowSurface::Declared,
        }
    }
}

pub struct ApplyResult {
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
/// 6. restrict the row to its declared surface
///
/// Step 6 is what makes this function the write path's answer too, not just
/// the read path's: seven of the nine row-returning write verbs already route
/// their `RETURNING` rows through here, so one stage covers all of them.
/// (The other five - `updateMany`, `deleteMany`, `purgeMany`, `restoreMany`
/// and the CAS fan-out - collapse their rows to a count and hand nothing to
/// JS.)
///
/// There is no cold-schema arm. Every stage below is driven by the descriptor
/// entry, and a collection this deploy's descriptor does not declare is refused
/// before the first row is touched — it is not served with the coercion,
/// decrypt and mask stages silently skipped. What used to be the "cold" case is
/// now the `collection_not_declared` error, and what used to be a warm read of
/// an EMPTY declared field map still behaves the same way it always did: the
/// platform system timestamps normalize, and no creator field is coerced.
///
/// `route` is a PARAMETER because step 5 issues SQL of its own - one SELECT per
/// (row, unmasked column), outside `exec` - and it has to run on the same
/// CONNECTION as the read that produced the rows. Every caller here already
/// holds a [`crate::tx_route::TxRoute`] and hands it over whole. Step 5 used to
/// resolve its own backend through the engine funnel, which read ADAPTER state
/// from an ENGINE file; the funnel now lives at `crate::tx_scope::ensure_backend`
/// and the value travels down instead.
///
/// **It was a `&BackendHandle` until 2026-09-03, and a handle is not a
/// connection.** Every caller passed `route.backend()`, which lost `in_tx` on
/// the way in, so step 5's SELECTs went to the autocommit lane while the rows
/// they were unmasking had come back from the transaction's. Taking the route
/// whole is what keeps the two together; see
/// [`super::unmask::dispatch_unmask_for_query`].
///
/// **The one backend reference left here is spelled
/// `route.backend().key_store()`, and reaching for
/// `crate::backend_handle::BackendHandle` by that path rather than the
/// `crate::backend` re-export is not cosmetic.** `backend/mod.rs` is
/// deliberately CONTESTED in `tests/lib/tier_direction_census.sh` - it has no
/// settled tier - so a reference wearing that path is neither judged nor
/// trusted: it lands in the census's DROPPED bucket, an ENGINE-to-ENGINE edge
/// the instrument cannot rule on. `backend_handle.rs` is tiered ENGINE. Do not
/// "simplify" a re-export path back in.
pub async fn apply(
    route: &crate::tx_route::TxRoute,
    binding: &DbBinding,
    collection: &str,
    mut rows: Vec<Value>,
    opts: ApplyOptions<'_>,
) -> Result<ApplyResult, DbError> {
    let app_id = binding.app_id();
    // The runtime data-access metadata (column types, encrypted
    // mode/keyId/wraps, mask kind/classification) comes from THE RUNTIME
    // DESCRIPTOR this isolate was built from. It used to come from a live
    // catalog read plus the migration engine's `zero-migrate:enc:` / `zero-migrate:mask:` column
    // comments - a round trip through the same DSL the descriptor is folded
    // from, which recovered a strict subset of it and cost one whole-schema
    // catalog walk per cold collection.
    //
    // The resolution is a `Result`, not an `Option`: a collection this deploy's
    // descriptor does not declare is refused, never served with the schema
    // stages silently skipped.
    let schema = scope_schema(
        crate::descriptor::collection_schema(binding, collection)?,
        &opts.schema_field_scope,
    );
    normalize_rows_on_read(&schema, &mut rows)?;

    if opts.apply_decrypt && super::schema_has_encrypted_columns(&schema) {
        // The key store comes off the handle this read ran on, not off a
        // second resolution of its own: `route` is already here for step 5,
        // and one handle per call is what keeps the decrypt keyed to the same
        // backend that returned the ciphertext.
        decrypt_rows_on_read(
            route.backend().key_store(),
            app_id,
            collection,
            &schema,
            &mut rows,
        )
        .await?;
    }

    let has_masked = if opts.wrap_masked && super::schema_has_masked_columns(&schema) {
        wrap_masked_rows_on_read(collection, &schema, &mut rows)?;
        true
    } else {
        false
    };

    if !opts.unmask_columns.is_empty() {
        super::unmask::dispatch_unmask_for_query(
            route,
            binding,
            collection,
            opts.unmask_columns,
            &mut rows,
        )
        .await?;
    }

    restrict_rows_to_surface(&schema, &opts.row_surface, &mut rows);

    Ok(ApplyResult { rows, has_masked })
}

/// Narrow a shared schema to the fields this read projected.
///
/// Takes and returns the cache's `Arc`. The `All` arm - the common one - now
/// hands the shared allocation straight through instead of deep-cloning the
/// schema on every read, which is what the old owned-`Value` signature forced.
/// The `Only` arm still copies, because it mutates.
fn scope_schema(schema: Arc<Value>, scope: &SchemaFieldScope<'_>) -> Arc<Value> {
    match scope {
        SchemaFieldScope::All => schema,
        SchemaFieldScope::Only(fields) => {
            let mut owned = (*schema).clone();
            let Some(obj) = owned.as_object_mut() else {
                return schema;
            };
            obj.retain(|key, _| key.starts_with('_') || fields.iter().any(|field| field == key));
            Arc::new(owned)
        }
    }
}

fn normalize_rows_on_read(schema: &Value, rows: &mut [Value]) -> Result<(), DbError> {
    for row in rows.iter_mut() {
        normalize_row_on_read(schema, row)?;
    }
    Ok(())
}

fn normalize_row_on_read(schema: &Value, row: &mut Value) -> Result<(), DbError> {
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
        Value::Null | Value::Bytes(_) => Ok(()),
        _ => Err(DbError::internal(
            "bytes column did not return native bytes",
        )),
    }
}

fn normalize_timestamp_value(value: &mut Value) -> Result<(), DbError> {
    match value {
        Value::Null | Value::Number(_) | Value::Timestamp(_) => Ok(()),
        Value::String(s) => {
            if let Some(ms) = parse_timestamp_millis(s) {
                *value = Value::Number(zeroship_data_query_builder::value::Number::from(ms));
            }
            Ok(())
        }
        other => Err(DbError::internal(format!(
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
/// `keys` is a PARAMETER, and it is a [`crate::encryption::KeyStore`] rather
/// than a backend because a key store is the whole of what this stage needs -
/// it issues no SQL, so it has no routing decision to make and must not be
/// handed one. It resolved its own backend through the engine funnel until
/// 2026-09-03, which read ADAPTER state (`crate::context`, `init_pool_async`)
/// from an ENGINE file. [`apply`] already holds the handle the read ran on, so
/// the store travels down from there and no key can come from a backend other
/// than the one that produced the ciphertext.
///
/// One arm, not two. This was a PG branch and a SQLite branch calling the SAME
/// function with the SAME arguments, differing only in the concrete type they
/// passed - a monomorphisation artifact of the `EncryptedColumn` trait, deleted
/// 2026-09-02. Column encryption never depended on the vendor; only key
/// sourcing did, and both backends source identically.
async fn decrypt_rows_on_read(
    keys: &crate::encryption::KeyStore,
    app_id: &str,
    collection: &str,
    schema: &Value,
    rows: &mut [Value],
) -> Result<(), DbError> {
    for row in rows.iter_mut() {
        crate::crud::encryption_pass::decrypt_row_on_read(keys, app_id, collection, schema, row)
            .await?;
    }
    Ok(())
}

/// Remove every key that is not on the row's declared surface.
///
/// The LAST stage. It used to be described here as "the one that closes the
/// `RETURNING *` leak", because twelve SQL sites in `zeroship-data-query-builder` emitted
/// `RETURNING *` - every physical column, including a masked field's raw
/// column - and none of them passed through the projection allowlist, which was
/// SELECT-side only. Without this stage `await db.users.insert({ ssn })` handed
/// the real value back under a key the generated `Row<S>` type does not
/// declare, invisible to any review written against the generated types.
///
/// **Those twelve sites now project explicitly**
/// (`zeroship_data_query_builder::compile::build_returning_expr`), so no statement this
/// runtime issues produces an off-surface key any more. **This stage is still
/// required**, and the reason has not changed: a statement is not the only
/// producer of a row. The WAL consumer decodes pgoutput with no schema in reach
/// and no projection to apply, and its rows arrive here with every physical
/// column on them. A projection binds one statement; this predicate binds every
/// row.
///
/// It runs last because the stages before it need the physical columns: the
/// decrypt stage reads ciphertext, and the mask pass strips the raw column
/// itself. A strip placed earlier would delete their input.
fn restrict_rows_to_surface(schema: &Value, surface: &RowSurface<'_>, rows: &mut [Value]) {
    let allowed = match surface {
        RowSurface::Declared => crate::compile::read_surface_columns(schema),
        RowSurface::Projected(names) => names.iter().cloned().collect(),
    };
    for row in rows.iter_mut() {
        if let Some(obj) = row.as_object_mut() {
            obj.retain(|key, _| allowed.contains(key.as_str()));
        }
    }
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
        let schema = zeroship_data_query_builder::value!({
            "active": { "type": "boolean" },
            "prefs": { "type": "object" },
            "avatar": { "type": "bytes" },
            "published_at": { "type": "date" }
        });
        let mut row = zeroship_data_query_builder::value!({
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}",
            "avatar": Value::Bytes(vec![104, 105]),
            "published_at": "2026-05-07T01:02:03.004Z"
        });

        normalize_row_on_read(&schema, &mut row).expect("normalize");

        assert_eq!(row["active"], Value::Bool(true));
        assert_eq!(
            row["prefs"],
            zeroship_data_query_builder::value!({"theme":"dark"})
        );
        assert_eq!(row["avatar"], Value::Bytes(vec![104, 105]));
        assert_eq!(
            row["published_at"],
            zeroship_data_query_builder::value!(1_778_115_723_004i64)
        );
    }

    #[test]
    fn normalize_row_on_read_skips_encrypted_columns() {
        let schema = zeroship_data_query_builder::value!({
            "secret": {
                "type": "bytes",
                "encrypted": {
                    "mode": "randomized",
                    "wraps": "bytes"
                }
            }
        });
        let mut row = zeroship_data_query_builder::value!({
            "secret": "AQID"
        });

        normalize_row_on_read(&schema, &mut row).expect("normalize");

        assert_eq!(row["secret"], Value::String("AQID".to_string()));
    }

    /// A descriptor entry that declares NO creator field still normalizes the
    /// platform system timestamps, and coerces nothing else.
    ///
    /// This used to be spelled `normalize_row_on_read(None, ...)` — "no schema
    /// at all". There is no such state any more: an undeclared collection is
    /// refused by `apply` before a row is touched, and the empty field map
    /// (`query::empty_read_schema()`) is the only remaining way to have zero
    /// declared fields. The behaviour the test pins is unchanged.
    #[test]
    fn normalize_row_on_read_without_declared_fields_only_normalizes_system_timestamps() {
        let mut row = zeroship_data_query_builder::value!({
            "created_at": "2026-05-07T01:02:03.004Z",
            "published_at": "2026-05-07T01:02:03.004Z",
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}"
        });

        normalize_row_on_read(&crate::compile::empty_read_schema(), &mut row).expect("normalize");

        assert_eq!(
            row["created_at"],
            zeroship_data_query_builder::value!(1_778_115_723_004i64)
        );
        assert_eq!(
            row["published_at"],
            Value::String("2026-05-07T01:02:03.004Z".to_string())
        );
        assert_eq!(row["active"], zeroship_data_query_builder::value!(1));
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
        let schema = zeroship_data_query_builder::value!({
            "active": { "type": "boolean" }
        });
        let mut row = zeroship_data_query_builder::value!({
            "active": 2
        });

        let err = normalize_row_on_read(&schema, &mut row)
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
        crate::cache_schema_for_tests(
            "app_aggregate_scope",
            "users",
            zeroship_data_query_builder::value!({
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

        let rows = vec![zeroship_data_query_builder::value!({
            "secret": 3
        })];

        let binding = DbBinding::cold_start("app_aggregate_scope");
        let alias = ["secret".to_string()];
        let rt = compio::runtime::Runtime::new().expect("compio runtime build");
        // `apply` takes the ROUTE rather than resolving a backend; none of the
        // three cases in this module reaches a statement through it (empty
        // `unmask_columns`, no encrypted column in scope), so any real route
        // does. Opened INSIDE the runtime, which its CDC publisher's `spawn`
        // requires - see `crate::test_support::unit_route`.
        //
        // ORACLE NOTE: installing a real handle COST this test its second,
        // free oracle. With no backend installed, a regression in the
        // `SchemaFieldScope::Only(&[])` narrowing reached the decrypt stage and
        // failed loudly on `not_configured`. It now reaches a working backend
        // instead, so the `assert_eq!` on `result.rows` below is the ONLY thing
        // that rules on the narrowing. Do not weaken it.
        let (route, dir) = rt.block_on(async { crate::test_support::unit_route(binding.app_id()) });
        let result = rt
            .block_on(apply(
                &route,
                &binding,
                "users",
                rows.clone(),
                ApplyOptions {
                    unmask_columns: &[],
                    schema_field_scope: SchemaFieldScope::Only(&[]),
                    row_surface: RowSurface::Projected(&alias),
                    ..ApplyOptions::default()
                },
            ))
            .expect("aggregate aliases must bypass schema-driven transforms");

        assert_eq!(
            result.rows,
            vec![zeroship_data_query_builder::value!({ "secret": 3 })]
        );
        assert!(!result.has_masked);

        // The control, and the reason `RowSurface` has no permissive arm: a
        // caller that does NOT name its aliases loses them. That is the safe
        // direction - a dropped accumulator is a visible failure - and it is
        // what makes `Declared` a usable default for the other twelve sites.
        let defaulted = rt
            .block_on(apply(
                &route,
                &binding,
                "users",
                rows,
                ApplyOptions {
                    unmask_columns: &[],
                    schema_field_scope: SchemaFieldScope::Only(&[]),
                    ..ApplyOptions::default()
                },
            ))
            .expect("apply");
        assert_eq!(
            defaulted.rows,
            vec![zeroship_data_query_builder::value!({})]
        );

        // Drop route-then-directory explicitly. `unit_route` returns
        // `(TxRoute, TempDir)` and the route owns the backend; scope exit drops
        // a tuple pattern's bindings in reverse declaration order - `dir`
        // first, which would delete the directory out from under a still-open
        // backend. See the ordering note on
        // `crate::test_support::unit_backend`.
        drop(route);
        drop(dir);
    }

    #[test]
    fn apply_can_skip_mask_wrapping_for_distinct_scalars() {
        crate::cache_schema_for_tests(
            "app_distinct_masked",
            "users",
            zeroship_data_query_builder::value!({
                "email": {
                    "type": "string",
                    "mask": { "kind": "email", "classification": "pii" }
                }
            }),
        );

        let rows = vec![zeroship_data_query_builder::value!({
            "email": "a***@example.com"
        })];

        let binding = DbBinding::cold_start("app_distinct_masked");
        let rt = compio::runtime::Runtime::new().expect("compio runtime build");
        // Inside the runtime: see the sibling test above.
        //
        // ORACLE NOTE: same trade as the sibling. A real handle removed the
        // free `not_configured` oracle that a regression in the
        // `wrap_masked: false` narrowing used to trip, so the `assert_eq!` on
        // `result.rows` and the `has_masked` assertion below are the ONLY
        // things ruling on it.
        let (route, dir) = rt.block_on(async { crate::test_support::unit_route(binding.app_id()) });
        let result = rt
            .block_on(apply(
                &route,
                &binding,
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
            vec![zeroship_data_query_builder::value!({ "email": "a***@example.com" })]
        );
        assert!(!result.has_masked);

        // Route before directory: see the sibling test above.
        drop(route);
        drop(dir);
    }
}
