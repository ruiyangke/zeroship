//! ORM-owned SQLite composition with committed-change publication.

use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

use crate::backend::sqlite::SqliteBackend;
use crate::cdc::broker::BrokerChangeSink;
use zeroship_data_orm::encryption::LocalKeySource;
use zeroship_data_orm::error::DbError;

/// Open the selected SQLite backend with the production broker sink.
///
/// The connection factory supplies keys captured by the host. Backend
/// construction never reads V8 or per-isolate state.
pub async fn open_sqlite_backend(
    path: impl AsRef<Path>,
    key_source: LocalKeySource,
) -> Result<SqliteBackend, DbError> {
    SqliteBackend::open(path, std::sync::Arc::new(BrokerChangeSink), key_source).await
}

/// Test composition for the synchronous directory constructor.
#[cfg(test)]
pub fn new_sqlite_backend(
    db_dir: PathBuf,
    key_source: LocalKeySource,
) -> Result<SqliteBackend, DbError> {
    SqliteBackend::new(db_dir, std::sync::Arc::new(BrokerChangeSink), key_source)
}
