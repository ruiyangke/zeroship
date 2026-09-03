//! Engine-owned backend composition.

use std::path::Path;

#[cfg(any(test, feature = "test-helpers"))]
use std::path::PathBuf;

use zeroship_core::change_event::ChangeEvent;

use crate::backend::sqlite::SqliteBackend;
use crate::backend::sqlite::change_sink::{ChangeSink, DeliveryDisposition};
use zeroship_data_core::error::DbError;

/// Engine adapter from the SQLite-owned delivery port to the process broker.
#[derive(Debug, Clone, Copy)]
struct BrokerChangeSink;

impl ChangeSink for BrokerChangeSink {
    fn disposition(&self, app_id: &str) -> DeliveryDisposition {
        if crate::broker::is_app_suppressed(app_id) {
            DeliveryDisposition::Suppressed
        } else if crate::broker::is_schema_pending(app_id) {
            DeliveryDisposition::SchemaPending
        } else {
            DeliveryDisposition::Deliver
        }
    }

    fn publish(&self, event: &ChangeEvent) {
        crate::broker::publish(event);
    }
}

/// Open the selected SQLite backend with the production broker sink.
pub async fn open_sqlite_backend(path: impl AsRef<Path>) -> Result<SqliteBackend, DbError> {
    SqliteBackend::open(path, BrokerChangeSink).await
}

/// Test composition for the synchronous directory constructor.
#[cfg(any(test, feature = "test-helpers"))]
pub fn new_sqlite_backend(db_dir: PathBuf) -> Result<SqliteBackend, DbError> {
    SqliteBackend::new(db_dir, BrokerChangeSink)
}
