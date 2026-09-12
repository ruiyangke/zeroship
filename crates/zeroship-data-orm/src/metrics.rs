//! Record database usage through the host-supplied meter.
//!
//! ORM operation boundaries emit metrics after successful work, including search
//! and unmask operations. Driver implementations do not determine billing policy.
//! Metric names are shared with the pricing catalog and must change with it.

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
#[cfg(test)]
pub fn reset_for_tests() {
    stamp(None);
}
