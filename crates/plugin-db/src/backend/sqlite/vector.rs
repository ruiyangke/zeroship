//! SQLite vector helpers — `sqlite-vec` `vec0` virtual-table SQL
//! composition + base-table mirror triggers.
//!
//! **P4 PR 7** (`docs/proposals/p4-search-implementation-plan.md` §10
//! 2026-05-24 reassessment): swapped from the pure-Rust flat scan
//! the P4 PR 4 amendment shipped. The `sqlite-vec` Rust crate
//! compiles the C `vec0` extension statically
//! and registers it via `sqlite3_auto_extension`
//! (`session::register_sqlite_vec_once`); NO `.so` ships, the
//! bundled-SQLite invariant (design §1) is preserved.
//!
//! ## `vec0` model
//!
//! Each `t.vector(dims, { metric })` column gets a paired `vec0`
//! virtual table:
//!
//! ```sql
//! CREATE VIRTUAL TABLE IF NOT EXISTS "<app>"."<coll>__vec_<col>"
//!   USING vec0("<col>" float[<dims>] distance_metric=<cosine|l2>);
//! ```
//!
//! The base table still carries a `BLOB` column for the vector — that
//! is the canonical write surface (the SDK's `t.vector()` payload
//! lands there via the regular INSERT/UPDATE path; the CDC preupdate
//! hook observes the row image). AFTER triggers mirror `(rowid,
//! <col>)` into the `vec0` vtable so reads can JOIN base ⟷ vec0 on
//! rowid and rank by `MATCH` distance:
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

use crate::backend::VectorMetric;
use crate::error::DbError;
use crate::query::quote_ident;

/// Translate a [`VectorMetric`] into the `distance_metric=...` clause
/// fragment used in a `vec0` virtual-table constructor.
///
/// Returns `"cosine"` or `"l2"`. `InnerProduct` is not a vec0-native
/// metric — callers route through [`reject_inner_product`] before
/// reaching the SQL builders.
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) fn metric_keyword(metric: VectorMetric) -> &'static str {
    match metric {
        VectorMetric::Cosine => "cosine",
        VectorMetric::L2 => "l2",
        // Inner product reached this far: caller skipped the
        // reject_inner_product check. Default to L2 so the SQL still
        // parses, but the caller is buggy — this branch should be
        // unreachable in production.
        VectorMetric::InnerProduct => "l2",
    }
}

/// Reject [`VectorMetric::InnerProduct`] with a typed `DbError` on
/// SQLite. The PG arm continues to support all three metrics via
/// pgvector opclasses (`vector_ip_ops`).
pub(crate) fn reject_inner_product(metric: VectorMetric) -> Result<(), DbError> {
    if matches!(metric, VectorMetric::InnerProduct) {
        return Err(DbError::Configuration {
            code: "vector_unsupported_metric",
            message:
                "db: SQLite vector backend does not support inner-product distance \
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
/// When the cached schema declares masked columns, the base-table
/// projection expands away from `t.*` so masked fields read from the
/// `<col>_masked` sibling under their logical output name.
pub(crate) fn build_vector_search_sql(
    app_id: &str,
    collection: &str,
    column: &str,
    query_hex: &str,
    k: usize,
    where_expr: &str,
    schema_hint: Option<&serde_json::Value>,
) -> String {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qvtab = quote_ident(&vec_table_name(collection, column));
    let qcol = quote_ident(column);
    let select_expr =
        crate::query::build_masked_aware_select_expr_for_table_alias(schema_hint, "t");
    let extra_filter = if where_expr.is_empty() {
        String::new()
    } else {
        format!(" AND {where_expr}")
    };
    format!(
        "SELECT {select_expr}, v.distance AS _distance \
         FROM {qschema}.{qcoll} t \
         JOIN {qschema}.{qvtab} v ON t.rowid = v.rowid \
         WHERE v.{qcol} MATCH {query_hex} AND k = {k}{extra_filter} \
         ORDER BY v.distance"
    )
}

/// Build the `CREATE VIRTUAL TABLE IF NOT EXISTS … USING vec0(…)` DDL.
///
/// Shape:
/// ```sql
/// CREATE VIRTUAL TABLE IF NOT EXISTS "<app>"."<coll>__vec_<col>"
///   USING vec0("<col>" float[<dims>] distance_metric=<cosine|l2>)
/// ```
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) fn build_create_vec0_sql(
    app_id: &str,
    collection: &str,
    column: &str,
    dims: i32,
    metric: VectorMetric,
) -> String {
    let qschema = quote_ident(app_id);
    let qvtab = quote_ident(&vec_table_name(collection, column));
    let kw = metric_keyword(metric);
    // vec0's constructor parser doesn't accept double-quoted column
    // identifiers (it errors with "Could not parse '…'"). The column
    // name appears unquoted; identifier safety relies on the SDK's
    // `validate_field_name` upstream check (`[A-Za-z_][A-Za-z0-9_]*`)
    // — same alphabet pgvector's pg arm relies on. Embedded `"` and
    // whitespace would already have been rejected.
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {qschema}.{qvtab} \
         USING vec0({column} float[{dims}] distance_metric={kw})"
    )
}

/// Build the initial-population `INSERT INTO __vec_<col> SELECT FROM
/// <coll>` statement. Run exactly once, gated by a `sqlite_master`
/// presence probe (same pattern as FTS5; see
/// `fts::build_initial_population_sql` rustdoc).
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) fn build_initial_population_sql(
    app_id: &str,
    collection: &str,
    column: &str,
) -> String {
    let qschema = quote_ident(app_id);
    let qvtab = quote_ident(&vec_table_name(collection, column));
    let qcoll = quote_ident(collection);
    let qcol = quote_ident(column);
    format!(
        "INSERT INTO {qschema}.{qvtab} (rowid, {qcol}) \
         SELECT rowid, {qcol} FROM {qschema}.{qcoll} WHERE {qcol} IS NOT NULL"
    )
}

/// `AFTER INSERT` trigger mirroring `(rowid, <col>)` into the vec0
/// vtable. The body references the vec0 table without a schema
/// qualifier — the SQLite engine rule "table referenced inside a
/// trigger body cannot be qualified by the database name" applies
/// (see `fts::build_insert_trigger_sql` rustdoc).
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) fn build_insert_trigger_sql(
    app_id: &str,
    collection: &str,
    column: &str,
) -> String {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qvtab_unqual = quote_ident(&vec_table_name(collection, column));
    let qtrg = quote_ident(&format!("{collection}__vec_{column}_ai"));
    let qcol = quote_ident(column);
    format!(
        "CREATE TRIGGER IF NOT EXISTS {qschema}.{qtrg} \
         AFTER INSERT ON {qschema}.{qcoll} \
         WHEN NEW.{qcol} IS NOT NULL BEGIN \
         INSERT INTO {qvtab_unqual} (rowid, {qcol}) VALUES (NEW.rowid, NEW.{qcol}); END"
    )
}

/// `AFTER DELETE` trigger removing the mirrored row from the vec0
/// vtable. vec0 supports plain `DELETE FROM v WHERE rowid = ?` (it
/// internally owns the row, no external-content sentinel required —
/// contrast FTS5's `'delete'` command sentinel).
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) fn build_delete_trigger_sql(
    app_id: &str,
    collection: &str,
    column: &str,
) -> String {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qvtab_unqual = quote_ident(&vec_table_name(collection, column));
    let qtrg = quote_ident(&format!("{collection}__vec_{column}_ad"));
    format!(
        "CREATE TRIGGER IF NOT EXISTS {qschema}.{qtrg} \
         AFTER DELETE ON {qschema}.{qcoll} BEGIN \
         DELETE FROM {qvtab_unqual} WHERE rowid = OLD.rowid; END"
    )
}

/// `AFTER UPDATE OF <col>` trigger — scoped to the vector column so
/// unrelated UPDATEs don't churn the vec0 index. Delete-then-insert
/// pair on the same rowid: vec0 has no `UPDATE` syntax that touches
/// the vector column directly (a future vec0 release may add it; the
/// d/i pair is the documented pattern today).
#[cfg(any(test, feature = "test-helpers"))]
pub(crate) fn build_update_trigger_sql(
    app_id: &str,
    collection: &str,
    column: &str,
) -> String {
    let qschema = quote_ident(app_id);
    let qcoll = quote_ident(collection);
    let qvtab_unqual = quote_ident(&vec_table_name(collection, column));
    let qtrg = quote_ident(&format!("{collection}__vec_{column}_au"));
    let qcol = quote_ident(column);
    format!(
        "CREATE TRIGGER IF NOT EXISTS {qschema}.{qtrg} \
         AFTER UPDATE OF {qcol} ON {qschema}.{qcoll} BEGIN \
         DELETE FROM {qvtab_unqual} WHERE rowid = OLD.rowid; \
         INSERT INTO {qvtab_unqual} (rowid, {qcol}) \
           SELECT NEW.rowid, NEW.{qcol} WHERE NEW.{qcol} IS NOT NULL; END"
    )
}

/// Encode `&[f32]` as the raw little-endian byte buffer vec0's MATCH
/// operand expects. vec0's `vec_f32` constructor accepts a BLOB
/// whose length is `4 * dims` and reads the bytes as native-endian
/// `f32` — every workspace-targeted platform is little-endian, so
/// the per-element `f32::to_le_bytes` produces the canonical form.
pub(crate) fn vec_to_le_bytes(v: &[f32]) -> Vec<u8> {
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
    fn metric_keyword_maps_cosine_and_l2() {
        assert_eq!(metric_keyword(VectorMetric::Cosine), "cosine");
        assert_eq!(metric_keyword(VectorMetric::L2), "l2");
    }

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
    fn build_create_vec0_sql_shape_cosine() {
        let sql = build_create_vec0_sql("myapp", "docs", "embedding", 768, VectorMetric::Cosine);
        assert_eq!(
            sql,
            "CREATE VIRTUAL TABLE IF NOT EXISTS \"myapp\".\"docs__vec_embedding\" \
             USING vec0(embedding float[768] distance_metric=cosine)"
        );
    }

    #[test]
    fn build_create_vec0_sql_shape_l2() {
        let sql = build_create_vec0_sql("myapp", "docs", "embedding", 4, VectorMetric::L2);
        assert_eq!(
            sql,
            "CREATE VIRTUAL TABLE IF NOT EXISTS \"myapp\".\"docs__vec_embedding\" \
             USING vec0(embedding float[4] distance_metric=l2)"
        );
    }

    #[test]
    fn build_initial_population_sql_shape() {
        let sql = build_initial_population_sql("myapp", "docs", "embedding");
        assert_eq!(
            sql,
            "INSERT INTO \"myapp\".\"docs__vec_embedding\" (rowid, \"embedding\") \
             SELECT rowid, \"embedding\" FROM \"myapp\".\"docs\" WHERE \"embedding\" IS NOT NULL"
        );
    }

    #[test]
    fn build_insert_trigger_sql_shape() {
        let sql = build_insert_trigger_sql("myapp", "docs", "embedding");
        assert!(
            sql.contains("AFTER INSERT ON \"myapp\".\"docs\""),
            "trigger header references qualified base table: {sql}"
        );
        assert!(
            sql.contains("INSERT INTO \"docs__vec_embedding\""),
            "trigger body references vec0 vtable unqualified: {sql}"
        );
        assert!(
            sql.contains("WHEN NEW.\"embedding\" IS NOT NULL"),
            "trigger skips NULL vectors: {sql}"
        );
    }

    #[test]
    fn build_delete_trigger_sql_shape() {
        let sql = build_delete_trigger_sql("myapp", "docs", "embedding");
        assert!(
            sql.contains("AFTER DELETE ON \"myapp\".\"docs\""),
            "trigger header: {sql}"
        );
        assert!(
            sql.contains("DELETE FROM \"docs__vec_embedding\" WHERE rowid = OLD.rowid"),
            "trigger body deletes by rowid: {sql}"
        );
    }

    #[test]
    fn build_update_trigger_sql_shape() {
        let sql = build_update_trigger_sql("myapp", "docs", "embedding");
        assert!(
            sql.contains("AFTER UPDATE OF \"embedding\" ON \"myapp\".\"docs\""),
            "trigger scoped to vector column: {sql}"
        );
        assert!(
            sql.contains("DELETE FROM \"docs__vec_embedding\" WHERE rowid = OLD.rowid"),
            "trigger body deletes old vector: {sql}"
        );
        assert!(
            sql.contains("INSERT INTO \"docs__vec_embedding\""),
            "trigger body inserts new vector: {sql}"
        );
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
        let sql = build_vector_search_sql(
            "app1",
            "users",
            "embedding",
            "x'0011'",
            5,
            "",
            Some(&schema),
        );
        assert!(
            !sql.starts_with("SELECT t.*"),
            "vector search must not use t.* when masked columns exist: {sql}"
        );
        assert!(
            sql.contains(r#""t"."ssn_masked" AS "ssn""#),
            "vector search must read the masked sibling: {sql}"
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
