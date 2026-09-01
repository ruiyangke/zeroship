//! Raw usage metrics the data primitives emit, and the one place that emits them.
//!
//! **Metering is infrastructure.** The billing signal is platform-measured so
//! app code can neither forge nor suppress it: these are stamped by trusted Rust
//! at the operation boundary, in the SUCCESS ARM ONLY. There is no `env.meter`,
//! and creator code never reaches this module.
//!
//! # Why this is its own module
//!
//! These lived in `exec.rs` as private items until 2026-09-01, which made the
//! contract accidentally equal to "whatever flows through `exec::run_sql` and
//! `exec::exec_mutation`". Three operation families do not:
//!
//! - `VectorIndex::vector_search` and `SpatialIndex::spatial_near` reach the
//!   database through `PostgresBackend::query_roled_json`, not through `exec`;
//! - every unmask read and the unmask audit INSERT go through the roled scalar
//!   and statement entry points.
//!
//! All of them ran, all of them cost a query, and none of them was billed. The
//! metric names are a BILLING CONTRACT shared with `zeroship-metering` and the
//! control plane's pricing catalog, so they are policy rather than mechanism -
//! the same reasoning that put the DB-1 timeouts in [`crate::budgets`].
//!
//! # This module claims no tier, deliberately
//!
//! The NAMES are core-shaped: three `&'static str`s several tiers ask about.
//! [`emit_db_metric`] is not, because it pulls the meter handle from
//! [`crate::context`], whose own destination crate is unsettled. So the tier
//! census reports this file as unjudged, which is the honest answer rather than
//! a CORE arm that would assert a placement nobody has decided.
//!
//! That bound is why every call site today is at the ENGINE-tier OPERATION
//! boundary (`crud/`) rather than inside the vendor entry points that actually
//! run the statements. Emitting from `backend/pg_autocommit.rs` would catch
//! every statement structurally and forever - but it would be the PG tier
//! reaching up for the meter handle, re-forming the very cycle that module was
//! created to break. When `context` settles low enough, move the emit down and
//! delete the hand-placed call sites; until then they are the contract, and a
//! new operation that runs SQL without one is silently unbilled.
//!
//! # Adding a metric
//!
//! Do not. The three names below are the whole db vocabulary the rest of the
//! platform knows; `zeroship-metering` and the pricing catalog key on these
//! strings. A new name is a billing-contract change and needs the catalog to
//! learn it in the same patch, or the usage is recorded and never priced.

use crate::context;

/// One read operation: a query, a count, a search, a single-cell unmask fetch.
/// Counts OPERATIONS, not rows.
pub(crate) const DB_READS: &str = "db_reads";

/// One mutation operation: insert, update, delete, or the unmask audit append.
/// Counts OPERATIONS, not rows.
pub(crate) const DB_WRITES: &str = "db_writes";

/// Rows a mutation affected or returned. There is deliberately no `db_rows_read`
/// twin - the platform has never defined one, and inventing it here would record
/// usage the pricing catalog cannot price.
pub(crate) const DB_ROWS_WRITTEN: &str = "db_rows_written";

/// Emit a per-app db metric in the success arm.
///
/// Pulls the meter handle from the per-isolate context (stamped on
/// `DbPlugin::register`); a no-op when no meter is configured, which is the
/// test-harness case. Synchronous lock-free atomic bump - it adds no await and
/// cannot fail the operation, so a caller never has to decide whether a metering
/// failure should fail the query. It must not: usage is billing, not correctness.
pub(crate) fn emit_db_metric(app_id: &str, metric: &str, n: u64) {
    if n == 0 {
        return;
    }
    if let Some(h) = context::with(|c| c.meter_handle(app_id)) {
        h.record(metric, n);
    }
}
