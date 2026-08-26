//! Stage 2 — Plan.
//!
//! Introspects the live schema for the app, builds the declared
//! `CREATE TABLE` (with FK emission deferred for cross-table cold-start
//! cases — proposal B2), and diffs them. The result is a flat
//! `Vec<DiffOp>` already classified as additive / compatible /
//! destructive.

use serde_json::Value;

use super::bootstrap::RegisterContext;
use crate::backend::{DialectBuilder, SchemaIntrospect};
use crate::diff::{DiffOp, LiveSchema};
use crate::error::DbError;

/// Output of stage 2.
pub(crate) struct Plan {
    /// Every diff op, in declared order. Destructive ones are kept so
    /// the validate stage can audit them; apply skips them under strict.
    pub ops: Vec<DiffOp>,
}

/// Run stage 2.
///
/// `introspect_schema` is the heavy I/O — it pulls the column / index /
/// FK lists from the catalog for the entire app schema. Caching that
/// across registerModel calls is a future optimisation; today every
/// call pays the round-trip.
///
/// The Backend's associated `LiveSchema` is constrained to the diff
/// engine's [`LiveSchema`] so the orchestrator can hand the snapshot
/// straight to `compute_diff` without an adapter.
///
/// The bound is narrowed to
/// [`SchemaIntrospect<LiveSchema = LiveSchema>`] (rather than `Backend<…>`).
/// Plan only does live-schema introspection + row-count estimation —
/// the carved capability trait expresses exactly that. See
/// `docs/archive/p0-implementation-plan.md` §"PR 2" and
/// `docs/archive/db-system-design.md` §7.
pub(crate) async fn compute_plan<B: SchemaIntrospect<LiveSchema = LiveSchema> + DialectBuilder>(
    backend: &B,
    ctx: &RegisterContext,
    collection: &str,
    schema: &Value,
) -> Result<Plan, DbError> {
    let mut live = backend.introspect_schema(&ctx.app_id).await?;
    let rows_estimate = backend.estimate_row_count(&ctx.app_id, collection).await?;
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
    // NO CREATE TABLE IS RENDERED HERE ANY MORE.
    //
    // `compute_diff` takes the DDL only to hang it on the `CreateTable` op's
    // `sql` field, and nothing executes that field now that registerModel
    // applies no schema change (`validate::changes_schema`). Rendering it would
    // produce a statement whose only destination is an audit row describing a
    // migration this process is not going to run - a plausible-looking artifact
    // that no longer corresponds to anything, which is worse than an empty one.
    //
    // The op itself is KEPT. It is the signal that the table is missing, which
    // is what validate turns into the refusal the operator sees.
    let create_table = String::new();

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
