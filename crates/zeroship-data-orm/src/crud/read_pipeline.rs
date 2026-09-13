use crate::schema::FieldMap;
use std::sync::Arc;

use crate::value::Value;

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
    /// Readable declared fields and permitted synthetic result columns.
    Declared,
    /// Requested fields or aggregate result aliases.
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
/// 1. decode registered storage values
/// 2. normalize
/// 3. decrypt encrypted columns
/// 4. wrap masked columns
/// 5. apply per-query unmask overrides
/// 6. restrict the row to its declared surface
///
/// Every stage is descriptor-driven. Undeclared collections fail before a row
/// is touched. The whole route is passed because unmasking may issue reads and
/// must use the same transaction connection as the query that produced `rows`.
pub async fn apply(
    route: &crate::tx_route::TxRoute,
    binding: &DbBinding,
    collection: &str,
    mut rows: Vec<Value>,
    opts: ApplyOptions<'_>,
) -> Result<ApplyResult, DbError> {
    let app_id = binding.app_id();
    // Installed model metadata determines types and protection.
    let schema = scope_schema(
        crate::descriptor::collection_schema(binding, collection)?,
        &opts.schema_field_scope,
    );
    route.sql_registration().decode_rows(&schema, &mut rows)?;

    if opts.apply_decrypt && super::schema_has_encrypted_columns(&schema) {
        // Use the key store bound to the backend that returned the ciphertext.
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
fn scope_schema(schema: Arc<FieldMap>, scope: &SchemaFieldScope<'_>) -> Arc<FieldMap> {
    match scope {
        SchemaFieldScope::All => schema,
        SchemaFieldScope::Only(fields) => {
            let mut owned = (*schema).clone();
            owned.retain(|key, _| fields.iter().any(|field| field == key));
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
    schema: &FieldMap,
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

/// Restrict the public result after protection consumes internal identity and storage.
fn restrict_rows_to_surface(schema: &FieldMap, surface: &RowSurface<'_>, rows: &mut [Value]) {
    let allowed = match surface {
        RowSurface::Declared => crate::sql::mapping::read_surface_columns(schema),
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
    schema: &FieldMap,
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
        crate::tests::fixtures::cache_schema(
            "app_aggregate_scope",
            "users",
            crate::value!({
                "secret": {
                    "type": "string",
                    "encrypted": true,
                    "mask": {
                        "kind": "last4",
                        "classification": "spi"
                    }
                }
            }),
        );

        let rows = vec![crate::value!({
            "secret": 3
        })];

        let binding = DbBinding::cold_start("app_aggregate_scope");
        let alias = ["secret".to_string()];
        let rt = compio::runtime::Runtime::new().expect("compio runtime build");
        // `apply` takes the ROUTE rather than resolving a backend; none of the
        // three cases in this module reaches a statement through it (empty
        // `unmask_columns`, no encrypted column in scope), so any real route
        // does. Opened INSIDE the runtime, which its CDC publisher's `spawn`
        // requires - see `crate::tests::fixtures::unit_route`.
        //
        // ORACLE NOTE: installing a real handle COST this test its second,
        // free oracle. With no backend installed, a regression in the
        // `SchemaFieldScope::Only(&[])` narrowing reached the decrypt stage and
        // failed loudly on `not_configured`. It now reaches a working backend
        // instead, so the `assert_eq!` on `result.rows` below is the ONLY thing
        // that rules on the narrowing. Do not weaken it.
        let (route, dir) =
            rt.block_on(async { crate::tests::fixtures::unit_route(binding.app_id()) });
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

        assert_eq!(result.rows, vec![crate::value!({ "secret": 3 })]);
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
        assert_eq!(defaulted.rows, vec![crate::value!({})]);

        // Drop route-then-directory explicitly. `unit_route` returns
        // `(TxRoute, TempDir)` and the route owns the backend; scope exit drops
        // a tuple pattern's bindings in reverse declaration order - `dir`
        // first, which would delete the directory out from under a still-open
        // backend. See the ordering note on
        // `crate::tests::fixtures::unit_backend`.
        drop(route);
        drop(dir);
    }

    #[test]
    fn apply_can_skip_mask_wrapping_for_distinct_scalars() {
        crate::tests::fixtures::cache_schema(
            "app_distinct_masked",
            "users",
            crate::value!({
                "email": {
                    "type": "string",
                    "mask": { "kind": "email", "classification": "pii" }
                }
            }),
        );

        let rows = vec![crate::value!({
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
        let (route, dir) =
            rt.block_on(async { crate::tests::fixtures::unit_route(binding.app_id()) });
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
            vec![crate::value!({ "email": "a***@example.com" })]
        );
        assert!(!result.has_masked);

        // Route before directory: see the sibling test above.
        drop(route);
        drop(dir);
    }
}
