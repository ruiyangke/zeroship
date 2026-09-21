//! Whose schema an apply may write: the app's LIVE binding to the database it
//! named.
//!
//! # Why this is an admission and not a derivation
//!
//! The apply target used to be derivable from the caller - one app, one schema,
//! named after the app - so authorizing the app authorized the target. A
//! database is a project-owned entity that several apps may reach and that an
//! app may be revoked from, so the request names it and the service has to ask
//! whether this app still holds it. Authorization answers "may this principal
//! deploy this app"; this answers "does that app reach this database", and
//! neither implies the other.
//!
//! # It reads Control's rows, on Control's connection
//!
//! The binding topology is a control-plane fact. A tenant cluster carries the
//! roles a binding was converged into and nothing that says which app declared
//! them, so a check against the cluster could only re-derive what the roles
//! already imply. The predicate is
//! [`zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE`], the same one
//! Control serves bindings from and the same one deploy admits against.

use compio_postgres::NoTls;
use zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE;
use zeroship_core::DatabaseId;
use zeroship_id::AppId;

/// Reader for the app-to-database edge, on the control plane's DSN.
#[derive(Debug, Clone)]
pub struct BindingStore {
    dsn: String,
}

/// A failure deciding whether an app holds a database.
///
/// There is no "not bound" variant: absence is an answer, not a failure, and
/// the caller has to be able to tell the two apart. A store that reported an
/// unreachable control plane the same way it reports a revoked binding would
/// turn an outage into a refusal the creator cannot act on.
#[derive(Debug, thiserror::Error)]
pub enum BindingStoreError {
    #[error("connect to the control plane: {0}")]
    Connect(compio_postgres::Error),
    #[error("read the app's database bindings: {0}")]
    Query(compio_postgres::Error),
}

impl BindingStore {
    #[must_use]
    pub fn new(dsn: impl Into<String>) -> Self {
        Self { dsn: dsn.into() }
    }

    /// Does this app hold a LIVE binding to this database?
    ///
    /// # Errors
    ///
    /// [`BindingStoreError::Connect`] when the control plane will not connect,
    /// [`BindingStoreError::Query`] when it will not answer. Both are
    /// infrastructure and neither means "not bound".
    pub async fn holds_live_binding(
        &self,
        app: &AppId,
        database: &DatabaseId,
    ) -> Result<bool, BindingStoreError> {
        let (client, connection) = compio_postgres::connect(&self.dsn, NoTls)
            .await
            .map_err(BindingStoreError::Connect)?;
        compio::runtime::spawn(async move {
            if let Err(err) = connection.run().await {
                tracing::debug!(error = %err, "migrate-server: binding store connection ended");
            }
        })
        .detach();
        // The shared predicate, narrowed to ONE database. `$1` is the app and
        // the appended conjunct is `$2`, which is the numbering the shared
        // fragment documents.
        let rows = client
            .query_text_params(
                &format!("SELECT 1 {LIVE_BINDINGS_FROM_WHERE} AND b.database_id = $2 LIMIT 1"),
                &[app.as_str(), database.as_str()],
            )
            .await
            .map_err(BindingStoreError::Query)?;
        Ok(!rows.is_empty())
    }

    /// Answer whether the control plane is reachable, for `/readyz`.
    ///
    /// This service holds no shared client, so the probe opens a connection of
    /// its own; the readiness gate's TTL is what keeps an unauthenticated probe
    /// flood from becoming a connection flood.
    ///
    /// # Errors
    /// [`BindingStoreError::Connect`] when the control plane will not connect,
    /// [`BindingStoreError::Query`] when it will not answer.
    pub async fn probe(&self) -> Result<(), BindingStoreError> {
        let (client, connection) = compio_postgres::connect(&self.dsn, NoTls)
            .await
            .map_err(BindingStoreError::Connect)?;
        compio::runtime::spawn(async move {
            if let Err(err) = connection.run().await {
                tracing::debug!(error = %err, "migrate-server: readiness probe connection ended");
            }
        })
        .detach();
        client
            .check_connection()
            .await
            .map_err(BindingStoreError::Query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The composed statement narrows the shared predicate rather than
    /// replacing it, and the extra conjunct takes `$2`.
    #[test]
    fn the_composed_statement_keeps_every_liveness_conjunct() {
        let sql = format!("SELECT 1 {LIVE_BINDINGS_FROM_WHERE} AND b.database_id = $2 LIMIT 1");
        for conjunct in [
            "b.app_id = $1",
            "b.status = 'active'",
            "b.observed_generation >= b.generation",
            "d.status = 'active'",
            "b.database_id = $2",
        ] {
            assert!(sql.contains(conjunct), "missing {conjunct} in {sql}");
        }
    }
}
