//! Hot-retention sweeper for the control-plane append-only audit tables
//! `zeroship.app_audit` and `zeroship.authz_decisions`.
//!
//! Both tables carry a BEFORE DELETE / UPDATE / TRUNCATE tamper trigger
//! (`app_audit_block_tamper` / `authz_decisions_block_tamper`, defined in
//! `db/migrations/V0004__control.sql`) that rejects every mutation —
//! they are append-only. The triggers carve out exactly one sanctioned
//! deleter: a connection that has flagged itself with
//! `SET zeroship.audit_retention = 'on'`. This cron is that deleter. It is the
//! direct peer of `auth::cron::audit_retention` (which sweeps
//! `zeroship.audit_events` on the auth service) and shares the same GUC name so
//! the two services use one identical escape hatch.
//!
//! Without this sweep both tables would grow unbounded with no deleter — the
//! whole point of P12 is to give them the same sanctioned-retention escape
//! `audit_events` already has.
//!
//! Retention window: a single configurable horizon (default 12 months) applied
//! uniformly to both tables, matching the events-retention default. Unlike the
//! auth `audit_events` sweep there is no per-event-class bucketing here: these
//! are coarse operational/authorization audit trails, not the fine-grained
//! security/PII/debug taxonomy `audit_events` carries.

use std::sync::Arc;
use std::time::Duration;

use crate::registry::{Registry, RegistryError};

/// Default retention horizon in months — matches the `audit_events` 12-month
/// hot-tier default. Rows older than this are swept on each tick.
pub const DEFAULT_RETENTION_MONTHS: u32 = 12;

/// Default sweep cadence in seconds (1 h). The sweep is cheap (one indexed
/// DELETE per table — both have a `occurred_at` index) so an hourly tick keeps
/// the tables close to their hot-tier shape without making the sweeper hot.
pub const DEFAULT_CHECK_SECS: u64 = 3600;

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then
/// sleeps `check_secs`.
///
/// Errors inside a single tick are logged and swallowed so a transient PG
/// hiccup doesn't kill the cron task (mirrors `auth::cron::audit_retention`).
//
// `compio_postgres::Client` holds a `!Send` connection handle; the lint is
// structural, not actionable.
#[allow(clippy::future_not_send)]
pub async fn run(registry: Arc<Registry>, retention_months: u32, check_secs: u64) {
    tracing::info!(
        retention_months,
        check_secs,
        "control audit_retention cron starting"
    );
    loop {
        if let Err(e) = tick(&registry, retention_months).await {
            tracing::error!(error = %e, "control audit_retention tick failed");
        }
        compio::time::sleep(Duration::from_secs(check_secs)).await;
    }
}

/// Run one retention sweep against both control audit tables. Exposed so a
/// live-PG integration test can drive a single tick deterministically without
/// sitting on the cron sleep.
///
/// Opens its own dedicated connection: the `zeroship.audit_retention` GUC and
/// the DELETEs it authorizes MUST run on the same connection, and a fresh
/// connection per tick guarantees no other code path inherits the flag. The
/// flag is cleared on every path (including mid-sweep error) before the
/// connection is dropped.
pub async fn tick(registry: &Registry, retention_months: u32) -> Result<(), RegistryError> {
    let conn = registry.conn().await?;

    // Flag this connection as the sanctioned deleter. The tamper triggers on
    // both tables permit DELETE only while this GUC reads 'on'. Application
    // handlers never set it (and can't via a parameterised query), so the
    // append-only guarantee still holds against app code and SQL injection.
    conn.batch_execute("SET zeroship.audit_retention = 'on'")
        .await
        .map_err(|e| RegistryError::Database(format!("audit retention: enable sweep: {e}")))?;

    let swept = sweep_all(&conn, retention_months).await;

    // Always clear the flag, even if a delete failed mid-sweep, before the
    // connection returns to the pool/driver.
    let _ = conn
        .batch_execute("SET zeroship.audit_retention = 'off'")
        .await;

    let (app_audit_deleted, authz_decisions_deleted) = swept?;
    let total = app_audit_deleted + authz_decisions_deleted;
    if total > 0 {
        tracing::info!(
            app_audit_deleted,
            authz_decisions_deleted,
            "control audit_retention sweep completed"
        );
    }
    Ok(())
}

/// Sweep both tables. Split out so [`tick`] can bracket it with the retention
/// GUC and still guarantee the flag is cleared on the error path.
async fn sweep_all(
    conn: &compio_postgres::Client,
    retention_months: u32,
) -> Result<(u64, u64), RegistryError> {
    let app_audit_deleted = delete_older_than(
        conn,
        "zeroship.app_audit",
        "app_audit",
        retention_months,
    )
    .await?;
    let authz_decisions_deleted = delete_older_than(
        conn,
        "zeroship.authz_decisions",
        "authz_decisions",
        retention_months,
    )
    .await?;
    Ok((app_audit_deleted, authz_decisions_deleted))
}

/// Delete rows of `table` whose `occurred_at` is older than `months` months.
/// Returns the deleted-row count.
///
/// The interval is built as `($1::text || ' months')::interval` because
/// PostgreSQL's interval-literal cast doesn't accept a parameterised numeric on
/// the LHS directly; binding months as text sidesteps the `postgres-types`
/// quirk. The table name is a fixed, code-controlled identifier (never user
/// input), so interpolating it into the statement is safe — it is one of the
/// two literals passed by `sweep_all`.
async fn delete_older_than(
    conn: &compio_postgres::Client,
    table: &str,
    label: &str,
    months: u32,
) -> Result<u64, RegistryError> {
    let months_str = i64::from(months).to_string();
    let sql = format!(
        "DELETE FROM {table} \
         WHERE occurred_at < NOW() - ($1::text || ' months')::interval"
    );
    let affected = conn
        .execute(&sql, &[&months_str])
        .await
        .map_err(|e| RegistryError::Database(format!("audit retention sweep ({label}): {e}")))?;
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_events_retention() {
        // The control tables inherit the same 12-month hot-tier default the
        // auth `audit_events` sweep uses.
        assert_eq!(DEFAULT_RETENTION_MONTHS, 12);
        // Hourly cadence keeps the cron cheap and the tables near hot shape.
        assert_eq!(DEFAULT_CHECK_SECS, 3600);
    }
}
