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

/// A creator session asked to reach a masked field's real value on a binding
/// that names no unmask role.
///
/// Refused for the reason [`unbound_session`] gives, applied to the statement
/// that carries the highest privilege the data plane can reach: running it
/// under whatever role the session already holds is the outcome that must not
/// be available, because the shared worker login is a member of every live
/// binding on the cluster.
fn unbound_unmask() -> DbError {
    DbError::config(
        "binding_not_resolved",
        "db: this connection narrows per binding, and the binding names no database to unmask in",
    )
}

/// The two role statements that bracket one audited raw-column read.
///
/// Composed as a pair so the statement that assumes the privilege and the one
/// that gives it back are derived from one binding in one place. Both names are
/// the ones the binding carries, quoted through the same path the setup batch
/// uses.
#[derive(Debug)]
pub(crate) struct UnmaskElevation {
    /// Assume the database's unmask role.
    pub(crate) assume: String,
    /// Narrow back to the binding role.
    pub(crate) restore: String,
}

/// Compose the elevation one audited raw-column read runs under, or `None` when
/// this connection's authority is the login's own.
///
/// `Connection` authority is a trusted native service reading its OWN schema on
/// a connection that already authenticates as the role owning it. There is no
/// per-binding narrowing to step out of and no unmask role to step into, so
/// there is nothing to bracket - which is the same answer
/// [`session_setup_sql`] gives that authority.
///
/// # Errors
///
/// [`DbError`] when the connection narrows per binding and the binding carries
/// no database edge to name either role.
pub(crate) fn unmask_elevation_sql(
    binding: &DbBinding,
    authority: SessionAuthority,
) -> Result<Option<UnmaskElevation>, DbError> {
    if authority == SessionAuthority::Connection {
        return Ok(None);
    }
    let unmask = binding.unmask_role().ok_or_else(unbound_unmask)?;
    let session = binding.session_role().ok_or_else(unbound_unmask)?;
    Ok(Some(UnmaskElevation {
        assume: format!(
            "SET LOCAL ROLE {}",
            crate::sql::mapping::quote_ident(unmask)
        ),
        restore: format!(
            "SET LOCAL ROLE {}",
            crate::sql::mapping::quote_ident(session)
        ),
    }))
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
    use zeroship_core::database_role::DatabaseCapability;
    use zeroship_core::{BindingId, DatabaseId};

    fn creator_binding() -> DbBinding {
        DbBinding::to_database(
            "app_demo",
            "deploy_demo",
            DatabaseId::mint(),
            BindingId::mint(),
            DatabaseCapability::ReadWrite,
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

    /// The EDGE rides in the role name, so the batch changes when it moves.
    ///
    /// Its control is the same edge composed twice, which must produce the same
    /// batch: without it this would pass for a batch that varied with anything
    /// at all.
    #[test]
    fn the_batch_names_the_edge_the_binding_was_resolved_at() {
        let database = DatabaseId::mint();
        let mine = BindingId::mint();
        let theirs = BindingId::mint();
        assert_ne!(mine, theirs, "the control: two mints are two edges");
        let compose = |edge: &BindingId| {
            DbBinding::to_database(
                "app_demo",
                "d",
                database.clone(),
                edge.clone(),
                DatabaseCapability::ReadWrite,
            )
            .unwrap()
        };

        let batch = |binding: &DbBinding| {
            tx_session_setup_sql(binding, SessionAuthority::PerBindingRole).unwrap()
        };
        assert_ne!(batch(&compose(&mine)), batch(&compose(&theirs)));
        assert_eq!(batch(&compose(&mine)), batch(&compose(&mine)));
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

    /// The PERMISSIVE direction, which nothing else in the tree asserts.
    ///
    /// Its sibling above binds the LOUD direction: a platform binding under
    /// `PerBindingRole` refuses, and you find that immediately. A creator
    /// binding under `Connection` does the opposite - it SUCCEEDS, and drops
    /// the `SET LOCAL ROLE` that is the tenant fence. Nothing fails, the data
    /// reads, and the boundary is simply not enforced.
    ///
    /// That is why the two constructors are named rather than defaulted, and
    /// this asserts the difference they name, so a call site that picks the
    /// wrong one is caught by a test rather than by a reviewer's attention.
    #[test]
    fn connection_authority_drops_the_narrowing_a_creator_binding_would_get() {
        let creator = creator_binding();
        let narrowed = tx_session_setup_sql(&creator, SessionAuthority::PerBindingRole)
            .expect("a creator binding names a role to narrow to");
        assert!(
            narrowed.contains("SET LOCAL ROLE"),
            "per-binding authority must narrow: {narrowed}"
        );
        let kept = tx_session_setup_sql(&creator, SessionAuthority::Connection)
            .expect("connection authority composes with no role of its own");
        assert!(
            !kept.contains("SET LOCAL ROLE"),
            "connection authority keeps the login's role, so it must NOT narrow: {kept}"
        );
    }

    /// The bracket names the binding's two roles, and the restore is the role
    /// the setup batch already narrowed to.
    ///
    /// The last assertion is the one that matters: a restore composed from the
    /// unmask role would look identical in every other respect and would leave
    /// the creator's next statement elevated.
    #[test]
    fn the_unmask_bracket_assumes_the_database_role_and_restores_the_binding() {
        let binding = creator_binding();
        let elevation = unmask_elevation_sql(&binding, SessionAuthority::PerBindingRole)
            .expect("a creator binding names both roles")
            .expect("per-binding authority brackets the read");
        assert_eq!(
            elevation.assume,
            format!(
                r#"SET LOCAL ROLE "{}""#,
                binding.unmask_role().expect("a creator binding unmasks")
            )
        );
        assert_eq!(
            elevation.restore,
            format!(
                r#"SET LOCAL ROLE "{}""#,
                binding.session_role().expect("a creator binding narrows")
            )
        );
        assert_ne!(
            elevation.assume, elevation.restore,
            "a bracket whose two halves were one statement would elevate and \
             never give the privilege back"
        );
    }

    /// Both halves are `SET LOCAL`, so an abandoned lease cannot carry the
    /// elevation into the next borrower.
    #[test]
    fn both_halves_of_the_unmask_bracket_are_transaction_scoped() {
        let binding = creator_binding();
        let elevation = unmask_elevation_sql(&binding, SessionAuthority::PerBindingRole)
            .unwrap()
            .unwrap();
        for statement in [&elevation.assume, &elevation.restore] {
            assert!(
                statement.starts_with("SET LOCAL ROLE "),
                "a session-level SET would outlive the transaction: {statement}"
            );
        }
    }

    /// A binding that names no database is refused, and the login's own
    /// authority brackets nothing.
    ///
    /// The two arms differ in one variable. Without the `Connection` arm the
    /// refusal would be indistinguishable from "this composer always refuses a
    /// platform binding", and without the `PerBindingRole` arm a composer that
    /// always answered `None` would pass.
    #[test]
    fn an_unbound_binding_is_refused_and_connection_authority_brackets_nothing() {
        let platform = DbBinding::platform(
            "platform",
            "fixture",
            SchemaName::new("zeroship").expect("fixture schema"),
        );
        let error = unmask_elevation_sql(&platform, SessionAuthority::PerBindingRole)
            .expect_err("a narrowing connection needs a role to assume");
        assert_eq!(error.code(), "binding_not_resolved", "{error}");
        assert!(
            unmask_elevation_sql(&platform, SessionAuthority::Connection)
                .expect("the login's own authority composes")
                .is_none(),
            "a trusted native service reading its own schema has nothing to assume"
        );
        assert!(
            unmask_elevation_sql(&creator_binding(), SessionAuthority::PerBindingRole)
                .expect("a creator binding composes")
                .is_some(),
            "the control: the refusal is about the binding, not about the composer"
        );
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
