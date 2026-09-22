//! PostgreSQL error classification into the backend-neutral [`DbError`]
//! hierarchy.
//!
//! **PG TIER.** SQLSTATE, `compio_postgres::Error`, PostgreSQL's source chain,
//! and the exact `SET LOCAL ROLE` failure shape are vendor dialect. They must
//! live beside the PostgreSQL backend, never in `crate::backend::postgres::error`, which owns only
//! the neutral error values and policy dispositions that every backend may use.
//!
//! The general translator is named [`classify`] so call sites read
//! `pg_error::classify(&error)`: the module supplies the vendor context and the
//! verb says that this is SQLSTATE classification rather than a structural
//! conversion. Keeping it as a free function is what lets this module move into
//! a PostgreSQL crate without pulling the vendor into the crate that owns
//! [`DbError`].

use zeroship_data_orm::error::{
    DbError, DenyReason, GRANT_REVOKED, GRANT_REVOKED_MESSAGE, NOT_MIGRATED_HINT,
    NOT_MIGRATED_MESSAGE, SCHEMA_NOT_MIGRATED, SessionSetupDisposition, SessionSetupError,
};

/// Classify an error returned by the binding's `SET LOCAL ROLE` batch.
///
/// Call this only at the two session-setup sites, after connection acquisition
/// and transaction start have succeeded. All other PostgreSQL errors, including
/// pool connection failures, must use [`classify`].
///
/// TWO outcomes, and the first is the whole runtime fence:
///
/// - **42501** means the role exists and this login may not assume it, which is
///   a revoked binding. Terminal: the reconciler withdrew the membership and
///   left the role standing precisely so this stays distinguishable from every
///   other way a `SET LOCAL ROLE` can fail.
/// - Anything else is unclassified and reaches [`classify`]. A database nothing
///   has converged answers **22023 `invalid_parameter_value`** here - the
///   generic "bad GUC value" code, shared with `SET statement_timeout = 'yes'` -
///   and it carries no creator remedy the platform can state, so it is not
///   given one.
///
/// Call it only where the setup batch just issued `SET LOCAL ROLE` for a
/// creator binding: provenance is the whole guard, because 42501 from any other
/// statement is an ordinary privilege refusal and not a withdrawn membership.
pub(crate) fn classify_pg_binding_session_setup(
    e: &compio_postgres::Error,
) -> SessionSetupError {
    if e.code() == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE) {
        let msg = walk_pg_chain(e);
        tracing::warn!(
            error = %msg,
            "binding role membership was revoked"
        );
        return SessionSetupError::new(
            SessionSetupDisposition::Denied(DenyReason::GrantRevoked),
            DbError::PermissionDenied {
                code: GRANT_REVOKED,
                message: GRANT_REVOKED_MESSAGE,
            },
        );
    }

    SessionSetupError::new(SessionSetupDisposition::Failed, classify(e))
}

/// Classify a `compio_postgres::Error` by SQLSTATE. Falls back to
/// [`DbError::Transient`] when the error has no code (e.g. connection-layer
/// errors that aren't class 08). Walks the source chain so the message reaching
/// JS includes the underlying error body, not the bare wrapper kind.
pub fn classify(e: &compio_postgres::Error) -> DbError {
    use compio_postgres::error::SqlState;

    let msg = walk_pg_chain(e);

    let Some(code) = e.code() else {
        // No SQLSTATE -- usually a connection-layer error
        // (transport, protocol, decode). Treat as transient.
        return DbError::Transient { message: msg };
    };

    if code == &SqlState::UNIQUE_VIOLATION {
        DbError::UniqueViolation {
            message: scrub_constraint_detail(msg),
        }
    } else if code == &SqlState::FOREIGN_KEY_VIOLATION {
        DbError::FkViolation {
            message: scrub_constraint_detail(msg),
        }
    } else if code == &SqlState::NOT_NULL_VIOLATION {
        DbError::NotNullViolation {
            message: scrub_constraint_detail(msg),
        }
    } else if code == &SqlState::CHECK_VIOLATION {
        DbError::CheckViolation {
            message: scrub_constraint_detail(msg),
        }
    } else if code == &SqlState::T_R_SERIALIZATION_FAILURE
        || code == &SqlState::T_R_DEADLOCK_DETECTED
    {
        DbError::Serialization { message: msg }
    } else if code == &SqlState::LOCK_NOT_AVAILABLE || code == &SqlState::OBJECT_IN_USE {
        DbError::LockContention { message: msg }
    } else if code == &SqlState::UNDEFINED_TABLE || code == &SqlState::UNDEFINED_COLUMN {
        // The session already narrowed, so the binding resolved and the grant
        // stands: what is absent is the relation, which is what an apply puts
        // there. A converged but unmigrated database reaches exactly here, and
        // so does a build asking for a column its migrations never added.
        DbError::config_hinted(SCHEMA_NOT_MIGRATED, NOT_MIGRATED_MESSAGE, NOT_MIGRATED_HINT)
    } else if code == &SqlState::DISK_FULL
        || code == &SqlState::OUT_OF_MEMORY
        || code == &SqlState::CONNECTION_EXCEPTION
        || code == &SqlState::CONNECTION_DOES_NOT_EXIST
        || code == &SqlState::CONNECTION_FAILURE
        || code == &SqlState::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION
        || code == &SqlState::SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION
    {
        DbError::Transient { message: msg }
    } else {
        // Unknown SQLSTATE -- leave classification to the catch-all
        // but preserve the message so the SDK can debug.
        DbError::Internal { message: msg }
    }
}

/// Classify a PostgreSQL error and prepend a `"<context>: "` phrase to the
/// resulting message body. The SQLSTATE-derived `.code` is preserved.
pub fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    let mut err = classify(&e);
    zeroship_data_orm::error::prefix_message(&mut err, &format!("{context}: "));
    err
}

/// Translate the PG introspection module's contextual driver error into the
/// backend-neutral error hierarchy.
pub(crate) fn classify_schema_error(e: super::pg_introspect::SchemaError) -> DbError {
    coded_sql(&format!("diff: {}", e.context), e.source)
}

/// Drop the PostgreSQL `DETAIL` line from a constraint-violation message before
/// it reaches app JS. PostgreSQL puts the conflicting value there, turning a
/// unique/check probe into a value-exfiltration oracle for possibly masked
/// columns. The primary message and constraint name remain available.
fn scrub_constraint_detail(msg: String) -> String {
    match msg.find("\nDETAIL:").or_else(|| msg.find("DETAIL:")) {
        Some(idx) => msg[..idx].trim_end().to_string(),
        None => msg,
    }
}

/// Walk the error source chain so the JS console sees the underlying
/// PostgreSQL error body rather than the bare wrapper kind.
fn walk_pg_chain(e: &compio_postgres::Error) -> String {
    let mut msg = format!("db: {e}");
    let mut cur: &dyn std::error::Error = e;
    while let Some(src) = std::error::Error::source(cur) {
        msg.push_str(&format!(" — caused by: {src}"));
        cur = src;
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_constraint_detail_drops_value_line_db18() {
        // The DETAIL line (with the conflicting value) is removed; the primary
        // message is kept for conflict handling.
        let raw = "db: duplicate key value violates unique constraint \"users_email_key\"\n\
                   DETAIL: Key (email)=(alice@example.com) already exists."
            .to_string();
        let scrubbed = scrub_constraint_detail(raw);
        assert!(
            !scrubbed.contains("alice@example.com"),
            "value must be scrubbed: {scrubbed}"
        );
        assert!(
            !scrubbed.contains("DETAIL"),
            "DETAIL line must be gone: {scrubbed}"
        );
        assert!(
            scrubbed.contains("unique constraint"),
            "primary message kept: {scrubbed}"
        );
        // A message without a DETAIL line is unchanged.
        let plain = "db: some other error".to_string();
        assert_eq!(scrub_constraint_detail(plain.clone()), plain);
    }

    // -----------------------------------------------------------------
    // The setup-boundary taxonomy.
    //
    // `compio_postgres::Error` has no public constructor, so the SQLSTATE
    // arm itself is measured against a live server in
    // `crates/zeroship-data-orm/tests/postgres_binding_fence.rs`. What is
    // pinned here is what a binding NAMES and what the codes are.
    // -----------------------------------------------------------------

    use crate::binding::DbBinding;
    use zeroship_core::database_role::DatabaseCapability;
    use zeroship_core::{BindingId, DatabaseId};

    /// The data plane and the cluster reconciler compose one role name.
    ///
    /// They are different processes creating and assuming the same object. If
    /// they disagreed, `SET LOCAL ROLE` would name an identifier no reconciler
    /// created and every creator transaction would fail at session setup.
    ///
    /// Two oracles: the name the binding carries, and the name
    /// `zeroship_migrate_server::datastore::cluster::binding_role` grants. The
    /// third arm is the statement the data plane actually sends, so this is a
    /// fact about the batch rather than helper-to-helper agreement.
    #[test]
    fn the_data_plane_and_the_reconciler_compose_one_binding_role() {
        let edge = BindingId::mint();
        let binding = DbBinding::to_database(
            "app_parity",
            "d",
            DatabaseId::mint(),
            edge.clone(),
            DatabaseCapability::ReadWrite,
        )
        .expect("the fixture ids compose");

        let granted = zeroship_migrate_server::datastore::cluster::binding_role(&edge)
            .expect("the reconciler composes the same name");
        assert_eq!(
            binding.session_role(),
            Some(granted.as_str()),
            "the data plane would narrow to a role the reconciler never created"
        );

        let setup_sql = crate::backend::postgres::pg_session_sql::tx_session_setup_sql(
            &binding,
            crate::connection::SessionAuthority::PerBindingRole,
        )
        .expect("tx setup sql");
        assert!(
            setup_sql.starts_with(&format!(
                "SET LOCAL ROLE {};",
                crate::sql::mapping::quote_ident(&granted)
            )),
            "session setup does not name the role the reconciler granted: {setup_sql}"
        );
    }

    /// Two edges are two roles, so the setup batch one sends is not the batch
    /// the other sends.
    ///
    /// The role is what a revoke withdraws, so two bindings narrowing to one
    /// name would make revoking either refuse both. Its control is the same
    /// edge composed twice, which must produce one name - without it this would
    /// pass over a composer that returned a fresh string every call.
    #[test]
    fn two_edges_narrow_to_two_roles_and_one_edge_to_one() {
        let database = DatabaseId::mint();
        let compose = |edge: &BindingId| {
            DbBinding::to_database(
                "app_e",
                "d",
                database.clone(),
                edge.clone(),
                DatabaseCapability::ReadWrite,
            )
            .expect("composes")
        };
        let mine = BindingId::mint();
        let theirs = BindingId::mint();
        assert_ne!(mine, theirs, "the control: two mints are two edges");

        assert_ne!(
            compose(&mine).session_role(),
            compose(&theirs).session_role(),
            "two edges on one database must be two roles"
        );
        assert_eq!(
            compose(&mine).session_role(),
            compose(&mine).session_role(),
            "one edge must be one role"
        );
    }

    /// The setup taxonomy has two outcomes and they are distinct.
    ///
    /// One is a SQLSTATE classification at the setup boundary; the other is
    /// decided before a statement is sent, because a role name exists only
    /// because a binding carries a database edge. Collapsing them would make an
    /// unbound app indistinguishable from a revoked binding, and only one of
    /// those is terminal.
    #[test]
    fn the_two_setup_outcomes_are_distinct() {
        use zeroship_data_orm::error::GRANT_REVOKED;

        let unbound = crate::backend::postgres::pg_session_sql::tx_session_setup_sql(
            &DbBinding::platform(
                "platform",
                "fixture",
                crate::sql::SchemaName::new("zeroship").expect("fixture schema"),
            ),
            crate::connection::SessionAuthority::PerBindingRole,
        )
        .expect_err("a narrowing connection needs a role to narrow to");

        assert_ne!(
            GRANT_REVOKED,
            unbound.code(),
            "the setup taxonomy must not collapse two conditions onto one code"
        );
    }

    /// Pin the explicit free-function translator shape. A driver `From` impl
    /// would put the vendor back into the crate that owns [`DbError`].
    #[test]
    fn pg_errors_are_classified_through_the_pg_tier_free_function() {
        fn assert_classifier(f: fn(&compio_postgres::Error) -> DbError) -> bool {
            std::ptr::fn_addr_eq(f, classify as fn(&compio_postgres::Error) -> DbError)
        }

        assert!(assert_classifier(classify));
    }
}
