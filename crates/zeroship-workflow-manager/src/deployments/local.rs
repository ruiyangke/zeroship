//! Local host provisioning of the normal app deployment catalog.

use super::Error;
use super::{collections, DeploymentHolds, SQLITE_SCHEMA};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, error::DbError, orm::Database, ConnectOptions,
};

impl DeploymentHolds {
    /// Open the local host's normal deployment index beside its manifests and
    /// blobs. Production hosts use their already provisioned platform database.
    ///
    /// # Errors
    /// Refuses incompatible catalogs without altering existing data.
    pub async fn open_local(path: &Path) -> Result<Self, Error> {
        initialize(path)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let database = loop {
            let result = Database::connect(
                DbBinding::new(
                    "platform",
                    "app-deployments",
                    SchemaName::new("main").map_err(|_| super::invalid_storage())?,
                ),
                ConnectOptions::new(
                    format!("sqlite:{}", path.display()),
                    ProjectKeySource::unavailable(),
                )
                .connection_authority(),
                collections()?,
            )
            .await;
            match result {
                Ok(database) => break database,
                // Another host may still hold bootstrap's schema read while
                // the ORM switches the database to WAL. Retry only contention.
                Err(DbError::LockContention { .. }) if Instant::now() < deadline => {
                    compio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        Self::new(database)
    }
}

fn initialize(path: &Path) -> Result<(), Error> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|_| unavailable())?;
    }
    let mut connection = rusqlite::Connection::open(path).map_err(|_| unavailable())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| unavailable())?;
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|_| unavailable())?;
    let actual = objects(&tx)?;
    if actual.is_empty() {
        tx.execute_batch(SQLITE_SCHEMA).map_err(|_| unavailable())?;
    } else {
        // Compare SQLite's own stored DDL against the compiler output. This is
        // bootstrap validation, not a second runtime schema or migration path.
        let expected = rusqlite::Connection::open_in_memory().map_err(|_| unavailable())?;
        expected
            .execute_batch(SQLITE_SCHEMA)
            .map_err(|_| unavailable())?;
        if actual != objects(&expected)? {
            return Err(Error::Unavailable(
                "local app deployment catalog schema is incompatible".into(),
            ));
        }
    }
    tx.commit().map_err(|_| unavailable())
}

fn objects(connection: &rusqlite::Connection) -> Result<Vec<(String, String)>, Error> {
    let mut query = connection
        .prepare(
            "SELECT name, sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT GLOB 'sqlite_*' ORDER BY name",
        )
        .map_err(|_| unavailable())?;
    let result = query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|_| unavailable())?
        .collect::<Result<_, _>>()
        .map_err(|_| unavailable());
    result
}

fn unavailable() -> Error {
    Error::Unavailable("local app deployment catalog is unavailable".into())
}
