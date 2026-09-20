//! The local host's platform metadata file.
//!
//! One `SQLite` file holds the normal deployment catalog and the manager's
//! queue, placement, scheduling and recovery records. Both schemas are
//! migration compiler output. Bootstrap installs them together into an empty
//! file and otherwise refuses a file whose stored DDL differs from that
//! combination, without altering it. Production hosts bind their already
//! provisioned platform databases instead; this module creates no customer
//! tables.

#![expect(
    clippy::future_not_send,
    reason = "platform bindings stay on their owning compio thread"
)]

use crate::{
    deployments::{self, DeploymentHolds},
    retention::CatalogClient,
    Options, Queue,
};
use std::{
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::schema_name::SchemaName;
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, error::DbError, orm::Database, ConnectOptions,
};

/// The deployment catalog followed by the manager schema, exactly as generated.
pub const SQLITE_SCHEMA: &str = concat!(
    include_str!("../schema/deployments/sqlite.sql"),
    include_str!("../schema/sqlite.sql"),
);

/// Bindings to the local host's single platform metadata file.
///
/// The deployment catalog and the queue use separate ORM bindings to the same
/// file. The queue acquires its deployment holds through this catalog.
/// How long opening waits on a file another local host still holds, such as
/// the process a restart replaces while it finishes shutting down.
const CONTENTION_BOUND: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct LocalPlatform {
    url: String,
    deployments: DeploymentHolds,
}

impl LocalPlatform {
    /// Install or verify the combined schema, then bind the deployment catalog.
    ///
    /// # Errors
    /// Refuses an unreadable file and any file whose schema is not exactly the
    /// combined catalog and manager schema, including a deployment-only catalog.
    pub async fn open(path: &Path) -> Result<Self, deployments::Error> {
        initialize(path)?;
        let url = format!("sqlite:{}", path.display());
        let deadline = Instant::now() + CONTENTION_BOUND;
        let database = loop {
            let result = Database::connect(
                binding()?,
                ConnectOptions::new(url.clone(), ProjectKeySource::unavailable())
                    .connection_authority(),
                deployments::collections()?,
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
        Ok(Self {
            url,
            deployments: DeploymentHolds::new(database)?,
        })
    }

    /// The normal app deployment catalog and its retention holds.
    #[must_use]
    pub const fn deployments(&self) -> &DeploymentHolds {
        &self.deployments
    }

    /// Bind the manager queue to this file. Queue holds use the local catalog.
    ///
    /// # Errors
    /// Refuses invalid queue bounds and unavailable storage.
    pub async fn queue(&self, options: Options) -> Result<Queue, crate::Error> {
        Queue::connect(
            binding().map_err(|_| crate::Error::Invalid)?,
            &self.url,
            options,
            Rc::new(CatalogClient::new(self.deployments.clone())),
        )
        .await
    }
}

fn binding() -> Result<DbBinding, deployments::Error> {
    Ok(DbBinding::platform(
        "platform",
        "local-platform",
        SchemaName::new("main").map_err(|_| invalid())?,
    ))
}

fn initialize(path: &Path) -> Result<(), deployments::Error> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|_| unavailable())?;
    }
    let mut connection = rusqlite::Connection::open(path).map_err(|_| unavailable())?;
    connection
        .busy_timeout(CONTENTION_BOUND)
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
            return Err(deployments::Error::Unavailable(
                "local platform metadata schema is incompatible".into(),
            ));
        }
    }
    tx.commit().map_err(|_| unavailable())
}

fn objects(connection: &rusqlite::Connection) -> Result<Vec<(String, String)>, deployments::Error> {
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

fn unavailable() -> deployments::Error {
    deployments::Error::Unavailable("local platform metadata is unavailable".into())
}

fn invalid() -> deployments::Error {
    deployments::Error::Internal("invalid local platform binding".into())
}

#[cfg(test)]
mod tests;
