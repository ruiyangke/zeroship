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
//! - `backend_handle::routed_vector_search` and `routed_spatial_near` reach the
//!   database through `PostgresBackend::query_roled_values` or the app's parked
//!   transaction client, not through `exec`;
//! - every unmask read and the unmask audit INSERT go through the roled scalar
//!   and statement entry points.
//!
//! All of them ran, all of them cost a query, and none of them was billed. The
//! metric names are a BILLING CONTRACT shared with `zeroship-metering` and the
//! control plane's pricing catalog, so they are policy rather than mechanism -
//! the same reasoning that put the DB-1 timeouts in [`crate::budgets`].
//!
//! # This module is ENGINE, and used to claim no tier at all
//!
//! It abstained until 2026-09-02 on the grounds that [`emit_db_metric`] pulled
//! the meter handle from the adapter tier's `context`, "whose own destination crate is
//! unsettled". Both halves of that expired: the 2026-09-02 ownership decision
//! settled the context as the adapter, and the meter no longer comes from it -
//! it is stamped onto this module's own thread-local by `DbPlugin::register`.
//! So the file is tiered, and the abstention would now HIDE an edge rather than
//! report one honestly.
//!
//! The METRIC NAMES are still core-shaped - three `&'static str`s several tiers
//! ask about - and could be re-homed lower if a second tier ever needs them.
//! Nothing does today.
//!
//! Every call site is at the ENGINE-tier OPERATION boundary (`crud/`) rather
//! than inside the vendor entry points that actually run the statements.
//! Emitting from `backend/pg_autocommit.rs` would catch every statement
//! structurally and forever - but it would be the PG tier reaching UP for the
//! meter, which is the cycle that module was created to break, and moving the
//! meter here did not change that. The hand-placed call sites are still the
//! contract, and a new operation that runs SQL without one is silently
//! unbilled.
//!
//! # Adding a metric
//!
//! Do not. The three names below are the whole db vocabulary the rest of the
//! platform knows; `zeroship-metering` and the pricing catalog key on these
//! strings. A new name is a billing-contract change and needs the catalog to
//! learn it in the same patch, or the usage is recorded and never priced.

use std::cell::RefCell;
use std::sync::Arc;

/// One read operation: a query, a count, a search, a single-cell unmask fetch.
/// Counts OPERATIONS, not rows.
pub const DB_READS: &str = "db_reads";

/// One mutation operation: insert, update, delete, or the unmask audit append.
/// Counts OPERATIONS, not rows.
pub const DB_WRITES: &str = "db_writes";

/// Rows a mutation affected or returned. There is deliberately no `db_rows_read`
/// twin - the platform has never defined one, and inventing it here would record
/// usage the pricing catalog cannot price.
pub const DB_ROWS_WRITTEN: &str = "db_rows_written";

/// Emit a per-app db metric in the success arm.
///
/// Pulls the meter handle from the per-isolate context (stamped on
/// `DbPlugin::register`); a no-op when no meter is configured, which is the
/// test-harness case. Synchronous lock-free atomic bump - it adds no await and
/// cannot fail the operation, so a caller never has to decide whether a metering
/// failure should fail the query. It must not: usage is billing, not correctness.
pub fn emit_db_metric(app_id: &str, metric: &str, n: u64) {
    if n == 0 {
        return;
    }
    if let Some(h) = handle_for(app_id) {
        h.record(metric, n);
    }
}

thread_local! {
    /// The process-wide meter, as this thread sees it.
    ///
    /// It lives HERE rather than as a field on the adapter's `ThreadDbContext`
    /// because this module is its only reader, and reading it through the
    /// adapter was the sole reason an engine-tier file reached up. The adapter
    /// still OWNS the decision - `DbPlugin::register` calls [`stamp`] - which is
    /// a downward call and therefore fine.
    ///
    /// `None` is the meter-less test harness, and the emit is then a no-op.
    static METER: RefCell<Option<Arc<zeroship_metering::Meter>>> = const { RefCell::new(None) };
}

/// Stamp the process-wide meter (called from `DbPlugin::register`).
///
/// Idempotent overwrite: registration may fire more than once per worker
/// thread, and every plugin on a thread shares one meter.
pub fn stamp(meter: Option<Arc<zeroship_metering::Meter>>) {
    METER.with_borrow_mut(|slot| *slot = meter);
}

/// A per-`app_id` handle over the stamped meter.
///
/// The handle binds the SERVER-INJECTED `app_id`, which is what stops a db op
/// metering another app; that binding is the reason this returns a handle
/// rather than the meter itself.
fn handle_for(app_id: &str) -> Option<zeroship_metering::MeterHandle> {
    METER.with_borrow(|m| {
        m.as_ref()
            .map(|m| zeroship_metering::MeterHandle::new(Arc::clone(m), app_id))
    })
}

/// Drop this thread's meter.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_tests() {
    stamp(None);
}
