//! Report database usage to the sink a host attaches to a binding.
//!
//! ORM operation boundaries report work after it succeeds, including search
//! and unmask operations. The ORM decides which operations count and names
//! their metrics; the host decides whom the usage is attributed to and where it
//! is recorded. A binding without a sink reports nothing. Driver
//! implementations do not report usage. Metric names are shared with the host's
//! pricing catalog and must change with it.

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

/// Receives the usage the ORM measures for one binding.
///
/// A host attaches a sink with [`crate::Database::with_usage_sink`], or passes
/// one to [`crate::tx_route::CapturedRoute::capture`] when it captures routes
/// itself. The ORM calls [`Self::record`] only after an operation succeeds and
/// never with a zero amount. It does not interpret the binding's identity on
/// the sink's behalf, so attribution and its validation belong to the host that
/// builds the sink.
pub trait UsageSink: std::fmt::Debug + Send + Sync {
    /// Add `amount` of `metric` to this sink's subject.
    fn record(&self, metric: &str, amount: u64);
}

/// Report successful work to the route's sink, if its host attached one.
pub(crate) fn emit_db_metric(sink: Option<&dyn UsageSink>, metric: &str, amount: u64) {
    if amount != 0 {
        if let Some(sink) = sink {
            sink.record(metric, amount);
        }
    }
}
