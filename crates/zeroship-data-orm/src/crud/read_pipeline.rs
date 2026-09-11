use std::sync::Arc;

use zeroship_data_sql::value::Value;

use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;

#[derive(Debug)]
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
#[derive(Debug)]
pub enum RowSurface<'a> {
    /// Declared fields + the seven system fields + the closed set of synthetic
    /// result columns. The default.
    Declared,
    /// An explicit name list. Aggregate result sets only: their keys are
    /// accumulator aliases, which no descriptor declares.
    Projected(&'a [String]),
}

#[derive(Debug)]
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

#[derive(Debug)]
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
    zeroship_data_sql::codecs::decode_rows(route.dialect(), &schema, &mut rows)?;

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
        crate::protection::encryption_pass::decrypt_row_on_read(
            keys, app_id, collection, schema, row,
        )
        .await?;
    }
    Ok(())
}

/// Remove every key that is not on the row's declared surface.
///
/// The LAST stage. It used to be described here as "the one that closes the
/// `RETURNING *` leak", because twelve SQL sites in `zeroship-data-sql` emitted
/// `RETURNING *` - every physical column, including a masked field's raw
/// column - and none of them passed through the projection allowlist, which was
/// SELECT-side only. Without this stage `await db.users.insert({ ssn })` handed
/// the real value back under a key the generated `Row<S>` type does not
/// declare, invisible to any review written against the generated types.
///
/// **Those twelve sites now project explicitly**
/// (`zeroship_data_sql::compile::build_returning_expr`), so no statement this
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
        crate::protection::mask_pass::wrap_row_on_read(schema, collection, row)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A descriptor entry that declares NO creator field still normalizes the
    /// platform system timestamps, and coerces nothing else.
    ///
    /// This used to be spelled `normalize_row_on_read(None, ...)` — "no schema
    /// at all". There is no such state any more: an undeclared collection is
    /// refused by `apply` before a row is touched, and the empty field map
    /// (`query::empty_read_schema()`) is the only remaining way to have zero
    /// declared fields. The behaviour the test pins is unchanged.

    #[test]
    fn scoped_schema_excludes_aggregate_alias_collisions() {
        crate::cache_schema_for_tests(
            "app_aggregate_scope",
            "users",
            zeroship_data_sql::value!({
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

        let rows = vec![zeroship_data_sql::value!({
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
            vec![zeroship_data_sql::value!({ "secret": 3 })]
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
        assert_eq!(defaulted.rows, vec![zeroship_data_sql::value!({})]);

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
            zeroship_data_sql::value!({
                "email": {
                    "type": "string",
                    "mask": { "kind": "email", "classification": "pii" }
                }
            }),
        );

        let rows = vec![zeroship_data_sql::value!({
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
            vec![zeroship_data_sql::value!({ "email": "a***@example.com" })]
        );
        assert!(!result.has_masked);

        // Route before directory: see the sibling test above.
        drop(route);
        drop(dir);
    }
}
