//! Engine-owned backend composition.

use std::path::Path;

#[cfg(any(test, feature = "test-helpers"))]
use std::path::PathBuf;

use zeroship_core::change_event::ChangeEvent;

use crate::backend::sqlite::SqliteBackend;
use crate::backend::sqlite::change_sink::{ChangeSink, DeliveryDisposition};
use zeroship_data_core::encryption::LocalKeySource;
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
///
/// **The key source is a parameter, not a lookup.** It was
/// `crate::context::isolate_key_source()` until 2026-09-03, which made this
/// ENGINE-tier composer read the ADAPTER's per-isolate context - the one edge
/// direction the split forbids. `PostgresBackend::new` took the same correction
/// one tier lower and for the same reason; its doc records the argument.
///
/// The caller that owns the context does the lookup. `init_pool_async` is the
/// only shipped one, and it is in the adapter, so the read is adapter-local.
pub async fn open_sqlite_backend(
    path: impl AsRef<Path>,
    key_source: LocalKeySource,
) -> Result<SqliteBackend, DbError> {
    SqliteBackend::open(path, BrokerChangeSink, key_source).await
}

// There is deliberately NO `open_postgres_backend` composer here.
//
// One was written on 2026-09-02 and removed the same hour: taking
// `Rc<compio_postgres::Pool>` put a vendor type in an ENGINE file's signature,
// which `tests/vendor_embedding_gate.sh` refused - correctly, and by name. The
// SQLite composer above is fine because its parameter is `impl AsRef<Path>`.
//
// So the Postgres arm injects at the CALL SITE instead: a caller that already
// holds a pool also passes [`crate::isolate_key_source`]. The vendor
// constructor takes what it needs, the engine never names the pool type, and
// the lookup still lives in exactly one function.

/// Test composition for the synchronous directory constructor.
#[cfg(any(test, feature = "test-helpers"))]
pub fn new_sqlite_backend(db_dir: PathBuf) -> Result<SqliteBackend, DbError> {
    SqliteBackend::new(db_dir, BrokerChangeSink, crate::context::isolate_key_source())
}
