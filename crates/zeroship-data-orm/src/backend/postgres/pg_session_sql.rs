//! Render PostgreSQL authority and resource-budget setup for ORM sessions.
//!
//! Role names derive from the physical schema provisioned by the migration service.
//! Timeout policy comes from `crate::budgets`; this module supplies SQL spelling.

use crate::connection::SessionAuthority;
use crate::sql::SchemaName;
use zeroship_core::database_role::per_app_role_name;

use zeroship_data_orm::budgets::{
    DB_IDLE_IN_TX_TIMEOUT_MS, DB_LOCK_TIMEOUT_MS, DB_STATEMENT_TIMEOUT_MS,
};
use zeroship_data_orm::error::DbError;

/// Compose the per-app role from the SCHEMA, the way the migration service does.
///
/// Both builders below go through this rather than calling
/// [`per_app_role_name`] on whatever they were handed, so the two data-plane
/// spellings cannot drift from each other - and neither can drift from
/// `zeroship_migrate_server::apply::runtime_role_provisioning_sql`, which takes
/// the same [`SchemaName`] type.
///
/// **The parameter's identity is the whole point.** The migration service
/// creates the role from the schema it created. A `&str` here would accept the
/// tenant id just as happily, and while the two are the same string that is
/// invisible; the day they diverge, `SET LOCAL ROLE` names a role nobody ever
/// created, every transaction fails at session setup, and
/// `pg_error::is_missing_per_app_session_role` stops matching - which turns an
/// actionable SCHEMA_NOT_PROVISIONED into a generic failure.
fn quoted_per_app_role(schema: &SchemaName) -> Result<String, DbError> {
    Ok(crate::sql::mapping::quote_ident(&per_app_role_name(
        schema.as_str(),
    )?))
}

/// Compose transaction-local authority and resource limits after `BEGIN`.
///
/// # Errors
///
/// Returns a typed error if a selected per-app role name is invalid.
pub(crate) fn tx_session_setup_sql(
    schema: &SchemaName,
    authority: SessionAuthority,
) -> Result<String, DbError> {
    session_setup_sql(schema, authority, true)
}

/// Compose setup for one statement in a short transaction. All settings are
/// local, so commit, rollback, and cancellation restore the pooled session.
///
/// # Errors
///
/// Returns a typed error if a selected per-app role name is invalid.
pub(crate) fn autocommit_local_session_setup_sql(
    schema: &SchemaName,
    authority: SessionAuthority,
) -> Result<String, DbError> {
    session_setup_sql(schema, authority, false)
}

fn session_setup_sql(
    schema: &SchemaName,
    authority: SessionAuthority,
    include_idle_timeout: bool,
) -> Result<String, DbError> {
    let mut statements = Vec::with_capacity(4);
    if authority == SessionAuthority::PerAppRole {
        statements.push(format!("SET LOCAL ROLE {}", quoted_per_app_role(schema)?));
    }
    statements.push(format!(
        "SET LOCAL statement_timeout = {DB_STATEMENT_TIMEOUT_MS}"
    ));
    if include_idle_timeout {
        statements.push(format!(
            "SET LOCAL idle_in_transaction_session_timeout = {DB_IDLE_IN_TX_TIMEOUT_MS}"
        ));
    }
    statements.push(format!("SET LOCAL lock_timeout = {DB_LOCK_TIMEOUT_MS}"));
    Ok(statements.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_schema() -> SchemaName {
        SchemaName::new("app_demo").expect("fixture schema name")
    }

    #[test]
    fn tx_session_setup_bounds_hold_and_statement_time() {
        // DB-1: every dedicated transaction client must SET LOCAL the timeout
        // guards that bound how long it can be held idle-in-transaction and how
        // long a statement may run — the defense against one tenant exhausting
        // the shared Postgres connection pool fleet-wide. SET LOCAL so they
        // revert at COMMIT/ROLLBACK.
        let sql = tx_session_setup_sql(&demo_schema(), SessionAuthority::PerAppRole).unwrap();
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
        // The short transaction bounds the statement and keeps every setting
        // scoped to this pool lease.
        let setup =
            autocommit_local_session_setup_sql(&demo_schema(), SessionAuthority::PerAppRole)
                .unwrap();
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
    fn connection_authority_keeps_the_login_role_and_applies_transaction_limits() {
        let setup = tx_session_setup_sql(
            &demo_schema(),
            crate::connection::SessionAuthority::Connection,
        )
        .unwrap();
        assert!(!setup.contains("ROLE"), "{setup}");
        assert!(setup.contains("SET LOCAL statement_timeout ="), "{setup}");
        assert!(
            setup.contains("SET LOCAL idle_in_transaction_session_timeout ="),
            "{setup}"
        );
        assert!(setup.contains("SET LOCAL lock_timeout ="), "{setup}");
    }

    #[test]
    fn both_session_setup_batches_refuse_overlong_role_names() {
        // 55 characters of `a` is a legal schema name and an ILLEGAL role name:
        // `app_` + 55 + `_role` is 64 bytes, one over PostgreSQL's limit. The
        // two validations are separate on purpose, so `SchemaName` accepting it
        // is not the composer accepting it.
        let schema = SchemaName::new(&"a".repeat(55)).expect("55 chars is a legal schema name");
        for result in [
            tx_session_setup_sql(&schema, SessionAuthority::PerAppRole),
            autocommit_local_session_setup_sql(&schema, SessionAuthority::PerAppRole),
        ] {
            let error = result.expect_err("64-byte role names must be refused");
            assert!(
                error.to_string().contains("64 bytes; maximum is 63 bytes"),
                "unexpected refusal: {error}"
            );
        }
    }

    /// Every value must be transaction scoped so it cannot survive pool reuse.
    #[test]
    fn every_setting_is_transaction_scoped() {
        for sql in [
            tx_session_setup_sql(&demo_schema(), SessionAuthority::PerAppRole).unwrap(),
            autocommit_local_session_setup_sql(&demo_schema(), SessionAuthority::PerAppRole)
                .unwrap(),
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
