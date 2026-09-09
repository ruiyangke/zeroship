//! The runtime descriptor is the data plane's SOLE schema authority.
//!
//! # What this module is
//!
//! One function, [`collection_schema`], and it is the only way any CRUD path
//! obtains a collection's shape. It returns the descriptor's own field map -
//! `{ <column>: FieldDef }`, `storage` block included - or a typed error. There
//! is no third state.
//!
//! # What it replaced
//!
//! `crud::introspect_schema` read the LIVE PostgreSQL catalog through the
//! reader now located at `backend::pg_introspect::read_live_schema` and re-derived
//! `{ type, encrypted?, mask? }` from the `zero-migrate:enc:` / `zero-migrate:mask:` column
//! comments the migration engine had written. That was:
//!
//! * **a round trip, not an independent source.** The sentinels are emitted by
//!   the migration engine out of the same DSL the descriptor is folded from, so
//!   the catalog could only ever agree with the descriptor or be stale.
//!
//!   **THAT BULLET IS TRUE OF SHAPE AND FALSE OF PROTECTION, and reading it as
//!   both is how issue #133 happened.** The two are folded from one DSL, but they
//!   are WRITTEN by different processes: the descriptor rides in the `.zship` the
//!   worker executes, and the sentinels are written by the migration service,
//!   which does not execute creator code. So they can disagree, and when they do
//!   the disagreement is not "stale" - it is the untrusted side claiming a column
//!   is unprotected. Measured 2026-09-04: deleting one `mask` key from a field
//!   made the next write store `987-65-4321` under the field's own name while
//!   `__zs_raw__ssn` sat NULL and the column's `zero-migrate:mask:` comment was still on
//!   it; deleting one `encrypted` key stored `hunter3-also-real` into the BYTEA
//!   column. [`crate::crud::protection_floor`] is the fence, and it reads the
//!   catalog for PRESENCE of a protection only. THE DESCRIPTOR REMAINS THE SOLE
//!   AUTHORITY ON SHAPE - the three reasons below are unaffected.
//! * **strictly poorer.** Its type mapper could not produce the `vector` or
//!   `geoPoint` tokens at all, and it never carried `vectorDims` or `idPrefix` -
//!   three facts live consumers need and one (`vectorDims`) whose absence is a
//!   hard `DbError::internal`. It also DROPPED the mask sibling's name, which
//!   that catalog reader had already recovered, so every consumer re-derived it
//!   by string formatting.
//! * **the only ungated production catalog read in the crate**, on the hot path
//!   of every read and every write, behind a per-thread singleflight and a
//!   process-wide cache that existed solely to amortise it.
//!
//! On SQLite it was never live at all: `runtime_schema_for` had no SQLite
//! introspector and fell straight back to the declared cache, so "the descriptor
//! is the sole authority" has been the shipped dev-tier behaviour all along.
//!
//! # Where the descriptor comes from
//!
//! `manifest.runtime_descriptor` -> the worker resolves the blob
//! (`crates/zeroship-worker/src/sync.rs`) -> `RuntimeState.runtime_descriptor`
//! -> runtime boot validates the complete value, injects
//! `globalThis.__zsRuntimeDescriptor` for the JavaScript SDK, and invokes
//! `DbPlugin::bind_runtime_descriptor` before creator modules evaluate. The DB
//! plugin publishes every collection's `fields` map to the thread-local store in
//! one synchronous replacement. The `storage` block therefore survives
//! unchanged, without a Rust -> JavaScript -> Rust registration round trip.
//!
//! The dev tier passes the generated descriptor to `zeroship serve`, which puts
//! it on `RuntimeState` and takes this same native boot path.

use std::sync::Arc;

use zeroship_data_query_builder::value::Value;

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;

/// The descriptor entry for one collection, or a typed error.
///
/// **There is no `Option` here on purpose.** The read path used to treat an
/// absent schema as "carry on", which is how L24 happened: the projection
/// allowlist stopped applying and the read-identifier check silently passed, so
/// a read served before the schema arrived returned every physical column and
/// accepted any field name. A collection the descriptor does not declare is not
/// a collection this isolate can serve, and saying so is the whole fix.
pub fn collection_schema(binding: &DbBinding, collection: &str) -> Result<Arc<Value>, DbError> {
    zeroship_data_core::schema_cache::with(|c| c.require(binding, collection))
}

/// Every collection this isolate's descriptor declares, with its field map.
pub fn declared_collections(binding: &DbBinding) -> Vec<(String, Arc<Value>)> {
    zeroship_data_core::schema_cache::with(|c| c.entries_for_binding(binding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_data_query_builder::value;

    #[test]
    fn an_undeclared_collection_is_a_typed_error_not_a_missing_schema() {
        crate::reset_engine_for_tests();
        let binding = DbBinding::cold_start("app_descriptor_miss");
        let err = collection_schema(&binding, "users").expect_err("must not resolve");
        assert!(
            format!("{err:?}").contains("collection_not_declared"),
            "an undeclared collection must carry the typed code; got {err:?}",
        );
    }

    /// The store is keyed by the DEPLOY. A worker thread holding a pinned and a
    /// current isolate of one app must not serve one deploy's schema to the
    /// other.
    ///
    /// This is the store-level half of the property.
    /// `v8_classes::db::tests::co_resident_deploy_bindings_keep_tokens_and_schema_entries_isolated`
    /// is the receiver-level half: it mints two real `Collection` wrappers off
    /// two real isolates and asks what each RESOLVES. Both are needed - this one
    /// would still pass if `mint_db` stopped capturing the deploy token, and
    /// that one would still pass if the key were right for the wrong reason.
    #[test]
    fn two_deploys_of_one_app_hold_separate_descriptor_entries() {
        crate::reset_engine_for_tests();
        let pinned = DbBinding::new(
            "app_two_deploys",
            "deploy_pinned",
            zeroship_data_query_builder::SchemaName::new("app_two_deploys").unwrap(),
        );
        let current = DbBinding::new(
            "app_two_deploys",
            "deploy_current",
            zeroship_data_query_builder::SchemaName::new("app_two_deploys").unwrap(),
        );
        zeroship_data_core::schema_cache::with_mut(|c| {
            c.insert_one(
                &pinned,
                "secrets",
                value!({ "marker": { "type": "string" } }),
            );
        });
        assert!(
            collection_schema(&current, "secrets").is_err(),
            "the current deploy must not read the pinned deploy's descriptor entry",
        );
        zeroship_data_core::schema_cache::with_mut(|c| {
            c.insert_one(
                &current,
                "secrets",
                value!({ "other": { "type": "string" } }),
            );
        });
        assert_eq!(
            collection_schema(&pinned, "secrets").unwrap().as_ref(),
            &value!({ "marker": { "type": "string" } }),
            "installing the current deploy redirected the pinned binding",
        );
    }
}
