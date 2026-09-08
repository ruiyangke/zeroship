//! SQLite vector helpers — `sqlite-vec` `vec0` MATCH query composition.
//!
//! This implementation (`docs/archive/p4-search-implementation-plan.md` §10,
//! 2026-05-24 reassessment) swapped from a pure-Rust flat scan
//! shipped previously. The `sqlite-vec` Rust crate
//! compiles the C `vec0` extension statically
//! and registers it via `sqlite3_auto_extension`
//! (`session::register_sqlite_vec_once`); NO `.so` ships, the
//! bundled-SQLite invariant (design §1) is preserved.
//!
//! ## `vec0` model — READ side only
//!
//! Each `t.vector(dims, { metric })` column is paired with a `vec0`
//! virtual table `"<app>"."<coll>__vec_<col>"` and three AFTER triggers
//! that mirror `(rowid, <col>)` into it. **This module composes none of
//! that DDL and neither does any other data-plane path** — schema
//! belongs to `zeroship-migrate`. The runtime descriptor NAMES the
//! shadow relation and its triggers (`AuxiliaryObject::ShadowTable`,
//! built by `auxiliary_objects` in
//! `zeroship-migrate-core/src/render/gen_types.rs`), which is what
//! [`vec_table_name`] must agree with; whether it EXISTS is the
//! migration's business. The builders that used to live here
//! (`build_create_vec0_sql`, `build_initial_population_sql`, the three
//! `build_*_trigger_sql`) were `#[cfg(any(test, feature =
//! "test-helpers"))]`, so no shipped binary ever ran them.
//!
//! BOUND, as of 2026-09-04, by the vector fixtures in
//! `crates/zeroship-plugin-db/tests/sqlite_integration.rs`. They used to
//! `format!` the shadow-relation name themselves - a THIRD spelling that
//! agreed with this one by luck while claiming to come from the
//! descriptor - and now render the descriptor through the migration
//! engine and create the relation under the name it records. A
//! divergence therefore makes the JOIN below name a relation that does
//! not exist, and every vector case fails.
//! `the_search_really_depends_on_the_name_the_engine_records` is the
//! control: it builds the relation ONE BYTE off the recorded name and
//! asserts the search refuses, so the passing cases are not passing for
//! any name at all.
//!
//! WHAT THAT DOES NOT COVER, measured: a COORDINATED rename of both
//! sides passes every arm of it, because the fixture follows the engine.
//! That case is caught instead by each crate's own literal pin -
//! `vec_table_name_pattern` here and `auxiliary_physical_objects_round_trip`
//! there - both of which a coordinated rename would also have to edit.
//! The residue is deliberate: unlike a timestamp spelling, this name
//! records no stored value, so two sides moving together is a rename
//! rather than a corruption.
//!
//! The base table carries the canonical `BLOB` column for the vector
//! (the SDK's `t.vector()` payload lands there via the regular
//! INSERT/UPDATE path; the CDC preupdate hook observes the row image).
//! Reads JOIN base <-> vec0 on rowid and rank by `MATCH` distance:
//!
//! ```sql
//! SELECT t.*, v.distance AS _distance
//!   FROM "<app>"."<coll>" t
//!   JOIN "<app>"."<coll>__vec_<col>" v ON t.rowid = v.rowid
//!  WHERE v."<col>" MATCH ?1 AND k = ?2
//!    AND <filter>
//!  ORDER BY v.distance;
//! ```
//!
//! `MATCH ?1` binds the query vector as a raw little-endian f32 byte
//! buffer (the `sqlite-vec` documented binding shape — same as
//! `zerocopy::AsBytes` over `Vec<f32>`).
//!
//! ## Metric translation
//!
//! `vec0` supports `distance_metric=cosine` and `distance_metric=l2`
//! at vtable-creation time (default L2 if omitted; see
//! <https://alexgarcia.xyz/sqlite-vec/features/knn.html>). Inner
//! product is NOT a vec0-native metric — [`VectorMetric::InnerProduct`]
//! surfaces as a typed `vector_unsupported_metric` error on SQLite.
//! Production inner-product workloads run on pgvector through the PG
//! arm (`vector_ip_ops` opclass).

use zeroship_data_core::error::DbError;
use zeroship_data_query_builder::compile::quote_ident;
use zeroship_data_query_builder::descriptors::VectorMetric;

/// Reject [`VectorMetric::InnerProduct`] with a typed `DbError` on
/// SQLite. The PG arm continues to support all three metrics via
/// pgvector opclasses (`vector_ip_ops`).
pub(crate) fn reject_inner_product(metric: VectorMetric) -> Result<(), DbError> {
    if matches!(metric, VectorMetric::InnerProduct) {
        return Err(DbError::Configuration {
            code: "vector_unsupported_metric",
            message: "db: SQLite vector backend does not support inner-product distance \
                 (vec0 supports cosine and l2 only). Use cosine for normalised \
                 embeddings; production inner-product workloads run on pgvector."
                .to_string(),
            hint: Some(
                "switch the column to { metric: 'cosine' } or run the app on the \
                 Postgres backend (pgvector)"
                    .to_string(),
            ),
        });
    }
    Ok(())
}

/// Compose the canonical `<collection>__vec_<column>` vec0 vtable name.
pub(crate) fn vec_table_name(collection: &str, column: &str) -> String {
    format!("{collection}__vec_{column}")
}

/// Build the vec0 nearest-neighbour search SQL.
///
/// The base-table projection is always an explicit list built from the
/// descriptor's field map: `t."id" AS "id"` plus one term per declared field,
/// with a masked field reading from its `storage.valueColumn` sibling under the
/// logical output name. There is no `t.*` arm — an undeclared identifier or a
/// non-object schema is a `QueryError`, surfaced here as a typed `DbError`.
pub(crate) fn build_vector_search_sql(
    app_id: &str,
    collection: &str,
    column: &str,
    query_hex: &str,
    k: usize,
    where_expr: &str,
    schema_hint: &serde_json::Value,
) -> Result<String, DbError> {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qvtab = quote_ident(&vec_table_name(collection, column));
    let qcol = quote_ident(column);
    let select_expr =
        zeroship_data_query_builder::compile::build_masked_aware_select_expr_for_table_alias(
            schema_hint,
            "t",
        )
        .map_err(DbError::from)?;
    let extra_filter = if where_expr.is_empty() {
        String::new()
    } else {
        format!(" AND {where_expr}")
    };
    Ok(format!(
        "SELECT {select_expr}, v.distance AS _distance \
         FROM {qschema}.{qcoll} t \
         JOIN {qschema}.{qvtab} v ON t.rowid = v.rowid \
         WHERE v.{qcol} MATCH {query_hex} AND k = {k}{extra_filter} \
         ORDER BY v.distance"
    ))
}

/// Encode `&[f32]` as the raw little-endian byte buffer vec0's MATCH
/// operand expects. vec0's `vec_f32` constructor accepts a BLOB
/// whose length is `4 * dims` and reads the bytes as native-endian
/// `f32` — every workspace-targeted platform is little-endian, so
/// the per-element `f32::to_le_bytes` produces the canonical form.
pub fn vec_to_le_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    //! Unit tests for the SQL primitives. The shapes are pinned
    //! against the canonical forms documented in this module's
    //! rustdoc + the upstream `sqlite-vec` examples.

    use super::*;

    #[test]
    fn reject_inner_product_returns_typed_error() {
        let err = reject_inner_product(VectorMetric::InnerProduct)
            .expect_err("inner product must reject");
        match err {
            DbError::Configuration { code, .. } => {
                assert_eq!(code, "vector_unsupported_metric");
            }
            other => panic!("expected Configuration error, got {other:?}"),
        }
        assert!(reject_inner_product(VectorMetric::Cosine).is_ok());
        assert!(reject_inner_product(VectorMetric::L2).is_ok());
    }

    #[test]
    fn vec_table_name_pattern() {
        assert_eq!(vec_table_name("docs", "embedding"), "docs__vec_embedding");
    }

    #[test]
    fn build_vector_search_sql_reads_masked_sibling_when_schema_cached() {
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "embedding": { "type": "vector" }
        });
        let sql = build_vector_search_sql("app1", "users", "embedding", "x'0011'", 5, "", &schema)
            .expect("a declared schema must build");
        assert!(
            !sql.starts_with("SELECT t.*"),
            "vector search must not use t.* when masked columns exist: {sql}"
        );
        // A masked column reads its OWN column, which holds the mask, and the
        // raw column must not appear in the projection at all.
        assert!(
            sql.contains(r#""t"."ssn" AS "ssn""#),
            "vector search must project the masked column: {sql}"
        );
        assert!(
            !sql.contains(&zeroship_data_query_builder::compile::raw_column_name(
                "ssn"
            )),
            "vector search must never name the raw column: {sql}"
        );
    }

    #[test]
    fn vec_to_le_bytes_is_native_f32_layout() {
        // 1.0f32 = 0x3F800000 → LE bytes [0x00, 0x00, 0x80, 0x3F].
        let bytes = vec_to_le_bytes(&[1.0]);
        assert_eq!(bytes, vec![0x00, 0x00, 0x80, 0x3F]);
        // Empty input → empty bytes.
        assert!(vec_to_le_bytes(&[]).is_empty());
        // Length is 4 × dims.
        assert_eq!(vec_to_le_bytes(&[1.0, 2.0, 3.0]).len(), 12);
    }
}
