//! Connection + executor configuration (design §2, §8).
//!
//! The executor runs **out-of-band at deploy** (not the request hot path) over
//! the bespoke **compio-postgres** driver — ZERO tokio, per the platform
//! invariant. This module owns the connection helper and the per-run
//! [`ExecutorConfig`] (which project, which schema, which meta schema, and the
//! mandatory `statement_timeout` / `lock_timeout` budgets from §1.5).

use std::time::Duration;

use compio_postgres::{Client, NoTls};

/// Error opening a migrator connection.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The underlying compio-postgres driver failed to connect.
    #[error("connect: {0}")]
    Connect(#[from] compio_postgres::Error),
}

/// Per-run executor configuration (design §2.3 / §1.5).
///
/// `statement_timeout` + `lock_timeout` are **mandatory** (§1.5: no indefinite
/// locks / `DoS`). They are applied per migration before its SQL runs.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// The project id (`prj_…`) — its bytes seed the apply-serializing advisory
    /// lock (`pg_advisory_lock(hashtext(project_id))`).
    pub project_id: String,
    /// The one schema this project's migrations own and may touch. Pinned into
    /// `search_path` for every apply, and the [`crate::guard::SqlGuard`]'s
    /// confinement target.
    pub project_schema: String,
    /// The per-project **meta schema** that holds the append-only
    /// `schema_migrations` journal (design §2.2). Separate from the project
    /// schema so a creator migration can't touch its own history.
    pub meta_schema: String,
    /// Mandatory per-statement timeout (§1.5). Maps to `SET statement_timeout`.
    pub statement_timeout: Duration,
    /// Mandatory per-statement lock-acquisition timeout (§1.5). Maps to
    /// `SET lock_timeout`.
    pub lock_timeout: Duration,
}

impl ExecutorConfig {
    /// A config with sane default timeouts for the named project + schema.
    ///
    /// The meta schema defaults to `<project_schema>_migrations` so it sits
    /// beside the project schema but is a distinct namespace.
    #[must_use]
    pub fn new(project_id: impl Into<String>, project_schema: impl Into<String>) -> Self {
        let project_schema = project_schema.into();
        let meta_schema = format!("{project_schema}_migrations");
        Self {
            project_id: project_id.into(),
            project_schema,
            meta_schema,
            // Conservative defaults; callers tune per deploy. Non-zero so a
            // runaway migration cannot hold locks indefinitely.
            statement_timeout: Duration::from_secs(60),
            lock_timeout: Duration::from_secs(30),
        }
    }

    /// `statement_timeout` in whole milliseconds (the unit `SET` takes).
    #[must_use]
    pub fn statement_timeout_ms(&self) -> u64 {
        u64::try_from(self.statement_timeout.as_millis()).unwrap_or(u64::MAX)
    }

    /// `lock_timeout` in whole milliseconds (the unit `SET` takes).
    #[must_use]
    pub fn lock_timeout_ms(&self) -> u64 {
        u64::try_from(self.lock_timeout.as_millis()).unwrap_or(u64::MAX)
    }
}

/// Open a migrator connection and spawn its driver loop on the compio runtime.
///
/// Mirrors the `connect` + `spawn(conn.run()).detach()` pattern used across
/// `crates/control` and `crates/auth`: the [`Connection`] half must be driven
/// for the [`Client`] to make progress, and on compio it runs as a detached
/// task on the current runtime.
///
/// # Errors
/// [`ConnectError::Connect`] if the driver cannot establish the session.
pub async fn connect(dsn: &str) -> Result<Client, ConnectError> {
    let (client, connection) = compio_postgres::connect(dsn, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "zeroship-migrate: pg connection loop ended with error");
        }
    })
    .detach();
    Ok(client)
}
