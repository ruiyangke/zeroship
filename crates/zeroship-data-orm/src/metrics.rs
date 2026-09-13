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

/// Record successful work through the handle validated before execution.
pub(crate) fn emit_db_metric(
    handle: Option<&zeroship_metering::MeterHandle>,
    metric: &str,
    n: u64,
) {
    if n != 0 {
        if let Some(handle) = handle {
            handle.record(metric, n);
        }
    }
}

thread_local! {
    // Hosts stamp the process meter before binding routes on this thread.
    static METER: RefCell<Option<Arc<zeroship_metering::Meter>>> = const { RefCell::new(None) };
}

/// Stamp the process-wide meter (called from `DbPlugin::register`).
///
/// Idempotent overwrite: registration may fire more than once per worker
/// thread, and every plugin on a thread shares one meter.
pub fn stamp(meter: Option<Arc<zeroship_metering::Meter>>) {
    METER.with_borrow_mut(|slot| *slot = meter);
}

/// Validate attribution before admitting a metered database operation.
pub(crate) fn bind(
    app_id: &str,
) -> Result<Option<zeroship_metering::MeterHandle>, crate::error::DbError> {
    METER.with_borrow(|meter| {
        meter
            .as_ref()
            .map(|meter| {
                let app_id = zeroship_core::AppId::parse(app_id).map_err(|error| {
                    crate::error::DbError::config("invalid_meter_app_id", error.to_string())
                })?;
                Ok(zeroship_metering::MeterHandle::new(
                    Arc::clone(meter),
                    app_id,
                ))
            })
            .transpose()
    })
}

/// Drop this thread's meter.
#[cfg(test)]
pub fn reset_for_tests() {
    stamp(None);
}
