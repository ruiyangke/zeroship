//! Stage 2 — Plan.
//!
//! Introspects `pg_catalog` for the live schema, builds the declared
//! `CREATE TABLE` (with FK emission deferred for cross-table cold-start
//! cases — proposal B2), and diffs them. The result is a flat
//! `Vec<DiffOp>` already classified as additive / compatible /
//! destructive.

use compio_postgres::Pool;
use serde_json::Value;

use super::bootstrap::RegisterContext;
use crate::diff::DiffOp;
use crate::query;

/// Output of stage 2.
pub(crate) struct Plan {
    /// Every diff op, in declared order. Destructive ones are kept so
    /// the validate stage can audit them; apply skips them under strict.
    pub ops: Vec<DiffOp>,
}

/// Run stage 2.
///
/// `read_live_schema` is the heavy I/O — it pulls the column / index /
/// FK lists from `pg_catalog` for the entire app schema. Caching that
/// across registerModel calls is a future optimisation; today every
/// call pays the round-trip.
pub(crate) async fn compute_plan(
    pool: &Pool,
    ctx: &RegisterContext<'_>,
    collection: &str,
    schema: &Value,
) -> Result<Plan, String> {
    let mut live = crate::diff::read_live_schema(pool, &ctx.app_id).await?;
    let rows_estimate = crate::diff::estimate_row_count(pool, &ctx.app_id, collection).await?;
    live.row_counts
        .insert(collection.to_string(), rows_estimate);

    // B2 — build CREATE TABLE with Deferred FK emission keyed on the live
    // table set. Refs to tables that already exist inline their FK; refs
    // to tables that don't exist yet skip the inline clause, and the diff
    // engine emits a follow-on `ALTER TABLE … ADD CONSTRAINT` op. This
    // breaks the cross-table cold-start race: concurrent
    // `registerModel("users")` / `registerModel("todos")` calls serialise
    // on the advisory lock; whichever runs second sees the first table in
    // `live` and can inline the FK, or defers it to its own apply phase.
    let existing_tables: std::collections::HashSet<String> =
        live.tables.keys().cloned().collect();
    let create_table = query::build_create_table_with_fks(
        &ctx.app_id,
        collection,
        schema,
        &query::FkEmission::Deferred(&existing_tables),
    )
    .map_err(|e| format!("db: {e}"))?;

    let ops = crate::diff::compute_diff(
        &live,
        &ctx.app_id,
        collection,
        schema,
        &create_table,
        &ctx.declared_indexes,
    );

    Ok(Plan { ops })
}
