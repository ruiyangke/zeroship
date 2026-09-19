//! Render PostgreSQL authority and resource-budget setup for ORM sessions.
//!
//! The role a session narrows to is the BINDING's, composed once on the
//! binding itself (`zeroship_data_orm::binding::DbBinding::to_database`) from
//! the binding id and the schema epoch. This module never composes a role: it
//! sends the one the binding carries, so the name the batch asks for is the
//! name `zeroship_migrate_server::datastore::cluster::grant_binding` created.
//!
//! Timeout policy comes from `crate::budgets`; this module supplies SQL
//! spelling.

use crate::binding::DbBinding;
use crate::connection::SessionAuthority;
use zeroship_data_orm::budgets::{
    DB_IDLE_IN_TX_TIMEOUT_MS, DB_LOCK_TIMEOUT_MS, DB_STATEMENT_TIMEOUT_MS,
};
use zeroship_data_orm::error::DbError;

/// A creator session asked to narrow, on a binding that names no role.
///
/// It is a refusal rather than a session that silently keeps the shared worker
/// login's authority: that login is a member of every live binding role on the
/// cluster, so a batch that skipped `SET LOCAL ROLE` would run with no fence at
/// all rather than with none of the privileges.
fn unbound_session() -> DbError {
    DbError::config(
        "binding_not_resolved",
        "db: this connection narrows per binding, and the binding names no database",
    )
}

/// Compose transaction-local authority and resource limits after `BEGIN`.
///
/// # Errors
///
/// [`DbError`] when the connection narrows per binding and the binding carries
/// no database edge to narrow to.
pub(crate) fn tx_session_setup_sql(
    binding: &DbBinding,
    authority: SessionAuthority,
) -> Result<String, DbError> {
    session_setup_sql(binding, authority, true)
}

/// Compose setup for one statement in a short transaction. All settings are
/// local, so commit, rollback, and cancellation restore the pooled session.
///
/// # Errors
///
/// [`DbError`] when the connection narrows per binding and the binding carries
/// no database edge to narrow to.
pub(crate) fn autocommit_local_session_setup_sql(
    binding: &DbBinding,
    authority: SessionAuthority,
) -> Result<String, DbError> {
    session_setup_sql(binding, authority, false)
}

fn session_setup_sql(
    binding: &DbBinding,
    authority: SessionAuthority,
    include_idle_timeout: bool,
) -> Result<String, DbError> {
    let mut statements = Vec::with_capacity(4);
    if authority == SessionAuthority::PerBindingRole {
        let role = binding.session_role().ok_or_else(unbound_session)?;
        statements.push(format!(
            "SET LOCAL ROLE {}",
            crate::sql::mapping::quote_ident(role)
        ));
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
    use crate::sql::SchemaName;
    use zeroship_core::{BindingId, DatabaseId};

    fn creator_binding() -> DbBinding {
        DbBinding::to_database(
            "app_demo",
            "deploy_demo",
            DatabaseId::mint(),
            BindingId::mint(),
            3,
        )
        .expect("the fixture ids compose a legal role name")
    }

    #[test]
    fn tx_session_setup_bounds_hold_and_statement_time() {
        // DB-1: every dedicated transaction client must SET LOCAL the timeout
        // guards that bound how long it can be held idle-in-transaction and how
        // long a statement may run — the defense against one tenant exhausting
        // the shared Postgres connection pool fleet-wide. SET LOCAL so they
        // revert at COMMIT/ROLLBACK.
        let binding = creator_binding();
        let sql = tx_session_setup_sql(&binding, SessionAuthority::PerBindingRole).unwrap();
        assert!(
            sql.contains(&format!(
                r#"SET LOCAL ROLE "{}""#,
                binding.session_role().expect("a creator binding narrows")
            )),
            "{sql}"
        );
        assert!(
            sql.contains("SET LOCAL idle_in_transaction_session_timeout ="),
            "{sql}"
        );
        assert!(sql.contains("SET LOCAL statement_timeout ="), "{sql}");
        assert!(sql.contains("SET LOCAL lock_timeout ="), "{sql}");
    }

    /// The role is the FIRST statement of the batch.
    ///
    /// PostgreSQL aborts a simple-query batch at its first failure, so a role
    /// statement anywhere but first would let the budgets apply under the
    /// login's own authority before the narrow was refused.
    #[test]
    fn the_binding_role_is_the_first_statement_of_both_batches() {
        let binding = creator_binding();
        let expected = format!(
            r#"SET LOCAL ROLE "{}""#,
            binding.session_role().expect("a creator binding narrows")
        );
        for sql in [
            tx_session_setup_sql(&binding, SessionAuthority::PerBindingRole).unwrap(),
            autocommit_local_session_setup_sql(&binding, SessionAuthority::PerBindingRole).unwrap(),
        ] {
            assert!(sql.starts_with(&expected), "{sql}");
        }
    }

    /// The epoch rides in the role name, so the batch changes when it moves.
    ///
    /// Its control is the same binding at the same epoch, which must produce
    /// the same batch: without it this would pass for a batch that varied with
    /// anything at all.
    #[test]
    fn the_batch_names_the_epoch_the_binding_was_resolved_at() {
        let database = DatabaseId::mint();
        let edge = BindingId::mint();
        let at_one =
            DbBinding::to_database("app_demo", "d", database.clone(), edge.clone(), 1).unwrap();
        let at_two =
            DbBinding::to_database("app_demo", "d", database.clone(), edge.clone(), 2).unwrap();
        let again =
            DbBinding::to_database("app_demo", "d", database, edge, 1).unwrap();

        let batch = |binding: &DbBinding| {
            tx_session_setup_sql(binding, SessionAuthority::PerBindingRole).unwrap()
        };
        assert_ne!(batch(&at_one), batch(&at_two));
        assert_eq!(batch(&at_one), batch(&again));
    }

    #[test]
    fn autocommit_local_session_setup_bounds_statement_time_via_set_local() {
        // The short transaction bounds the statement and keeps every setting
        // scoped to this pool lease.
        let binding = creator_binding();
        let setup =
            autocommit_local_session_setup_sql(&binding, SessionAuthority::PerBindingRole).unwrap();
        assert!(
            setup.contains(&format!(
                r#"SET LOCAL ROLE "{}""#,
                binding.session_role().expect("a creator binding narrows")
            )),
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
        let platform = DbBinding::platform(
            "platform",
            "fixture",
            SchemaName::new("zeroship").expect("fixture schema"),
        );
        let setup = tx_session_setup_sql(&platform, SessionAuthority::Connection).unwrap();
        assert!(!setup.contains("ROLE"), "{setup}");
        assert!(setup.contains("SET LOCAL statement_timeout ="), "{setup}");
        assert!(
            setup.contains("SET LOCAL idle_in_transaction_session_timeout ="),
            "{setup}"
        );
        assert!(setup.contains("SET LOCAL lock_timeout ="), "{setup}");
    }

    /// A connection that narrows per binding refuses a binding with no role,
    /// BEFORE any statement is sent.
    ///
    /// **This is the third arm of the taxonomy, and it lives here rather than in
    /// the classifier.** A role name exists only because a binding carries a
    /// database edge, and the classifier is only reached after a role name was
    /// sent - so "no live binding" cannot be a SQLSTATE outcome. The condition
    /// is decided here instead, with its own code, and it is terminal: no
    /// amount of retrying gives an app a database nobody bound it to.
    ///
    /// The control is the same binding under `Connection` authority, which must
    /// compose: the refusal is about the PAIRING, not about the binding.
    #[test]
    fn both_session_setup_batches_refuse_a_binding_that_names_no_database() {
        let platform = DbBinding::platform(
            "platform",
            "fixture",
            SchemaName::new("zeroship").expect("fixture schema"),
        );
        for result in [
            tx_session_setup_sql(&platform, SessionAuthority::PerBindingRole),
            autocommit_local_session_setup_sql(&platform, SessionAuthority::PerBindingRole),
        ] {
            let error = result.expect_err("a narrowing connection needs a role to narrow to");
            assert_eq!(error.code(), "binding_not_resolved", "{error}");
        }
        tx_session_setup_sql(&platform, SessionAuthority::Connection)
            .expect("control: the same binding composes under the login's own authority");
    }

    /// Every value must be transaction scoped so it cannot survive pool reuse.
    #[test]
    fn every_setting_is_transaction_scoped() {
        let binding = creator_binding();
        for sql in [
            tx_session_setup_sql(&binding, SessionAuthority::PerBindingRole).unwrap(),
            autocommit_local_session_setup_sql(&binding, SessionAuthority::PerBindingRole).unwrap(),
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
