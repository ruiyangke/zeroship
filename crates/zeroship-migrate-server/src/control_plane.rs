//! Whether the control plane is reachable, for `/readyz`.
//!
//! Every endpoint this service exposes reads or writes the control database -
//! authorization resolves a caller's project seat there, and the cluster
//! reconciler reads its declarations from it - so a process that cannot open it
//! cannot serve, whatever PostgreSQL says about the tenant cluster.

use compio_postgres::NoTls;

/// Opens the control plane's DSN. This service holds no shared client on the
/// request path, so the probe opens a connection of its own.
#[derive(Debug, Clone)]
pub struct ControlPlaneStore {
    dsn: String,
}

/// A failure reaching the control plane.
///
/// The two arms are kept apart because they are different operator problems: a
/// connection this process cannot open is a credential or a network, and a
/// query it cannot run on an open connection is a database.
#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("connect to the control plane: {0}")]
    Connect(compio_postgres::Error),
    #[error("query the control plane: {0}")]
    Query(compio_postgres::Error),
}

impl ControlPlaneStore {
    #[must_use]
    pub fn new(dsn: impl Into<String>) -> Self {
        Self { dsn: dsn.into() }
    }

    /// Answer whether the control plane is reachable, for `/readyz`.
    ///
    /// The readiness gate's TTL is what keeps an unauthenticated probe flood
    /// from becoming a connection flood.
    ///
    /// # Errors
    /// [`ControlPlaneError::Connect`] when the control plane will not connect,
    /// [`ControlPlaneError::Query`] when it will not answer.
    pub async fn probe(&self) -> Result<(), ControlPlaneError> {
        let (client, connection) = compio_postgres::connect(&self.dsn, NoTls)
            .await
            .map_err(ControlPlaneError::Connect)?;
        compio::runtime::spawn(async move {
            if let Err(err) = connection.run().await {
                tracing::debug!(error = %err, "migrate-server: readiness probe connection ended");
            }
        })
        .detach();
        client
            .check_connection()
            .await
            .map_err(ControlPlaneError::Query)
    }
}
