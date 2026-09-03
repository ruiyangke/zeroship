//! PostgreSQL per-app session setup: the `SET LOCAL` batch that narrows a
//! connection to one tenant's role and bounds how long it may hold resources.
//!
//! **PG TIER. This is dialect, not policy.** `SET LOCAL ROLE`,
//! `statement_timeout`, `idle_in_transaction_session_timeout` and `lock_timeout`
//! are PostgreSQL GUCs; SQLite has no equivalent and never will. The NUMBERS
//! these render are the opposite - cross-backend policy - and live in
//! [`zeroship_data_core::budgets`], because `transaction/driver.rs` derives BOTH backends'
//! protocol execution deadline from `DB_IDLE_IN_TX_TIMEOUT_MS`.
//!
//! Both functions lived in `auth/bootstrap.rs` until 2026-09-01, which the tier
//! census reads as ENGINE. That mattered for more than tidiness: the roled
//! autocommit funnel is moving down into this tier, and it calls
//! [`autocommit_local_session_setup_sql`]. Had the builder stayed engine-side,
//! the `PG -> ENGINE` cycle the move exists to break would simply re-form under
//! a different symbol, and the census would still refuse the split.
//!
//! Callers above may reach down here freely - ENGINE -> PG is rank 3 -> 2. What
//! must never happen is the reverse.

use zeroship_core::database_role::per_app_role_name;

use zeroship_data_core::budgets::{DB_IDLE_IN_TX_TIMEOUT_MS, DB_LOCK_TIMEOUT_MS, DB_STATEMENT_TIMEOUT_MS};
use zeroship_data_core::error::DbError;

/// Combined per-transaction client setup: `SET LOCAL ROLE` + the DB-1 timeout
/// guards, as one simple-query batch run right after `BEGIN`. All `SET LOCAL`,
/// so every value (role + timeouts) auto-reverts at COMMIT/ROLLBACK and can
/// never leak to a later checkout of the (dedicated, but defensively reset)
/// connection.
///
/// # Errors
///
/// Returns a typed database error if the complete role name exceeds
/// PostgreSQL's identifier limit.
pub fn tx_session_setup_sql(app_id: &str) -> Result<String, DbError> {
    let role = crate::query::quote_ident(&per_app_role_name(app_id)?);
    Ok(format!(
        "SET LOCAL ROLE {role}; \
         SET LOCAL statement_timeout = {DB_STATEMENT_TIMEOUT_MS}; \
         SET LOCAL idle_in_transaction_session_timeout = {DB_IDLE_IN_TX_TIMEOUT_MS}; \
         SET LOCAL lock_timeout = {DB_LOCK_TIMEOUT_MS}"
    ))
}

/// Combined autocommit (pooled) client setup, run inside a short-lived
/// explicit transaction: `SET LOCAL ROLE` + statement/lock timeout guards.
///
/// Every value is `SET LOCAL`, so role + timeouts auto-revert at
/// COMMIT/ROLLBACK — including the implicit rollback-on-drop the
/// `compio_postgres::Transaction` performs when the future is cancelled
/// mid-flight. This makes the pooled (autocommit) path leak-proof on
/// EVERY return-to-pool path, matching the explicit-transaction path's
/// guarantee. No `idle_in_transaction` guard — the wrapping transaction
/// is opened and committed around a single statement, so it never sits
/// idle in transaction (the per-statement `statement_timeout` already
/// bounds the work).
///
/// # Errors
///
/// Returns a typed database error if the complete role name exceeds
/// PostgreSQL's identifier limit.
pub(crate) fn autocommit_local_session_setup_sql(app_id: &str) -> Result<String, DbError> {
    let role = crate::query::quote_ident(&per_app_role_name(app_id)?);
    Ok(format!(
        "SET LOCAL ROLE {role}; \
         SET LOCAL statement_timeout = {DB_STATEMENT_TIMEOUT_MS}; \
         SET LOCAL lock_timeout = {DB_LOCK_TIMEOUT_MS}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The three tests below moved verbatim from `auth/bootstrap.rs` with the
    // functions they cover, on 2026-09-01. A test that stays behind when its
    // subject moves is how a module ends up asserting things it no longer owns.

    #[test]
    fn tx_session_setup_bounds_hold_and_statement_time() {
        // DB-1: every dedicated transaction client must SET LOCAL the timeout
        // guards that bound how long it can be held idle-in-transaction and how
        // long a statement may run — the defense against one tenant exhausting
        // the shared Postgres connection pool fleet-wide. SET LOCAL so they
        // revert at COMMIT/ROLLBACK.
        let sql = tx_session_setup_sql("app_demo").unwrap();
        assert!(
            sql.contains(r#"SET LOCAL ROLE "app_app_demo_role""#),
            "{sql}"
        );
        assert!(
            sql.contains("SET LOCAL idle_in_transaction_session_timeout ="),
            "{sql}"
        );
        assert!(sql.contains("SET LOCAL statement_timeout ="), "{sql}");
        assert!(sql.contains("SET LOCAL lock_timeout ="), "{sql}");
    }

    #[test]
    fn autocommit_local_session_setup_bounds_statement_time_via_set_local() {
        // P2-C1 + DB-1: the pooled autocommit path is pool-bounded (8) but a
        // slow statement still pins one of those shared connections -- bound it
        // with a statement_timeout. Crucially every value is `SET LOCAL`, run
        // inside an explicit transaction, so role + timeouts auto-revert at
        // COMMIT/ROLLBACK (including rollback-on-drop on cancellation) and can
        // never leak to the next checkout. No idle-in-tx guard — the wrapping
        // transaction commits around a single statement and never sits idle.
        let setup = autocommit_local_session_setup_sql("app_demo").unwrap();
        assert!(
            setup.contains(r#"SET LOCAL ROLE "app_app_demo_role""#),
            "{setup}"
        );
        assert!(setup.contains("SET LOCAL statement_timeout ="), "{setup}");
        assert!(setup.contains("SET LOCAL lock_timeout ="), "{setup}");
        // Every directive must be SET LOCAL — a bare session-level SET would
        // re-introduce the leak the explicit transaction is here to prevent.
        assert!(
            !setup.contains("SET ROLE "),
            "must be SET LOCAL ROLE: {setup}"
        );
        assert!(
            !setup.contains("idle_in_transaction"),
            "no idle guard on autocommit: {setup}"
        );
    }

    #[test]
    fn both_session_setup_batches_refuse_overlong_role_names() {
        let app_id = "a".repeat(55);
        for result in [
            tx_session_setup_sql(&app_id),
            autocommit_local_session_setup_sql(&app_id),
        ] {
            let error = result.expect_err("64-byte role names must be refused");
            assert!(
                error.to_string().contains("64 bytes; maximum is 63 bytes"),
                "unexpected refusal: {error}"
            );
        }
    }

    /// Every value must be `SET LOCAL`. A bare `SET` would survive COMMIT and
    /// ride the pooled connection to the next tenant - the leak class P2-C1
    /// removed.
    #[test]
    fn every_setting_is_transaction_scoped() {
        for sql in [
            tx_session_setup_sql("app_demo").unwrap(),
            autocommit_local_session_setup_sql("app_demo").unwrap(),
        ] {
            for stmt in sql.split(';') {
                let stmt = stmt.trim();
                assert!(
                    stmt.starts_with("SET LOCAL "),
                    "non-transaction-scoped setting would outlive the tx: {stmt}"
                );
            }
        }
    }
}
