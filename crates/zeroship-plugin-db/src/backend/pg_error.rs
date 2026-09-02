//! PostgreSQL error classification into the backend-neutral [`DbError`]
//! hierarchy.
//!
//! **PG TIER.** SQLSTATE, `compio_postgres::Error`, PostgreSQL's source chain,
//! and the exact `SET LOCAL ROLE` failure shape are vendor dialect. They must
//! live beside the PostgreSQL backend, never in `crate::error`, which owns only
//! the neutral error values and policy dispositions that every backend may use.
//!
//! The general translator is named [`classify`] so call sites read
//! `pg_error::classify(&error)`: the module supplies the vendor context and the
//! verb says that this is SQLSTATE classification rather than a structural
//! conversion. Keeping it as a free function is what lets this module move into
//! a PostgreSQL crate without pulling the vendor into the crate that owns
//! [`DbError`].

use zeroship_data_core::error::{
    DbError, DenyReason, GRANT_REVOKED, GRANT_REVOKED_MESSAGE, MISSING_ROLE_HINT,
    MISSING_ROLE_MESSAGE, SCHEMA_NOT_PROVISIONED, SessionSetupDisposition, SessionSetupError,
};

/// Does this server error match the role set by per-app session setup?
///
/// This discriminator is called only where the caller knows it just issued
/// `SET LOCAL ROLE` for `app_id`. Provenance is the primary guard; SQLSTATE
/// and the exact expected role name pin the measured server response.
///
/// The SQLSTATE alone is not enough. `SET LOCAL ROLE "missing"` reports **22023
/// `invalid_parameter_value`** (measured against postgres:16 -- `LOCATION:
/// call_string_check_hook, guc.c`), not 42704 `undefined_object`. 22023 is the
/// generic "bad GUC value" code, shared with `SET statement_timeout = 'yes'`,
/// so matching it alone would reclassify unrelated configuration failures as
/// creator-facing.
///
/// Matching the exact role derived from `app_id` is stronger than sniffing the
/// `app_<id>_role` shape. A pool DSN can use an app-shaped login name and return
/// a FATAL 28000 during reconnect; that is an operator connection failure, not
/// an app condition that `zeroship migrate` can repair.
fn is_missing_per_app_session_role(
    code: &compio_postgres::error::SqlState,
    primary_message: &str,
    app_id: &str,
) -> bool {
    use compio_postgres::error::SqlState;

    let Ok(expected_role) = zeroship_core::database_role::per_app_role_name(app_id) else {
        return false;
    };
    code == &SqlState::INVALID_PARAMETER_VALUE
        && primary_message == format!("role \"{expected_role}\" does not exist")
}

/// Classify an error returned by the per-app `SET LOCAL ROLE` batch.
///
/// Call this only at the two session-setup sites, after connection acquisition
/// and transaction start have succeeded. All other PostgreSQL errors, including
/// pool connection failures, must use [`classify`].
pub(crate) fn classify_pg_per_app_session_setup(
    e: &compio_postgres::Error,
    app_id: &str,
) -> SessionSetupError {
    if e.as_db_error()
        .is_some_and(|db| is_missing_per_app_session_role(db.code(), db.message(), app_id))
    {
        let msg = walk_pg_chain(e);
        tracing::warn!(
            error = %msg,
            "per-app database role missing; app has not been migrated"
        );
        return SessionSetupError::new(
            SessionSetupDisposition::Preserve,
            DbError::config_hinted(
                SCHEMA_NOT_PROVISIONED,
                MISSING_ROLE_MESSAGE,
                MISSING_ROLE_HINT,
            ),
        );
    }

    if e.code() == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE) {
        let msg = walk_pg_chain(e);
        tracing::warn!(
            error = %msg,
            "per-app database role membership was revoked"
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

/// Test-only view of the contextual classifier's creator-facing error.
/// Production transaction code also consumes the private disposition.
#[cfg(feature = "test-helpers")]
pub fn classify_pg_per_app_session_setup_for_tests(
    e: &compio_postgres::Error,
    app_id: &str,
) -> DbError {
    classify_pg_per_app_session_setup(e, app_id).into_db_error()
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
pub(crate) fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    let mut err = classify(&e);
    zeroship_data_core::error::prefix_message(&mut err, &format!("{context}: "));
    err
}

/// Translate the PG introspection module's contextual driver error into the
/// backend-neutral error hierarchy.
#[cfg(any(test, feature = "test-helpers"))]
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
    // `schema_not_provisioned` -- the missing per-app role classification.
    //
    // These drive `is_missing_per_app_session_role` directly rather than the
    // contextual converter because `compio_postgres::Error` has no public
    // constructor. The real converter path against a live server is covered by
    // `tests/missing_role.rs`; these pin the discriminator's cheap edge cases.
    // -----------------------------------------------------------------

    #[test]
    fn per_app_role_composers_match_across_services() {
        use compio_postgres::error::SqlState;

        let app_id = "role_parity";
        let role = zeroship_core::database_role::per_app_role_name(app_id)
            .expect("parity fixture role name");
        let quoted_role = crate::query::quote_ident(&role);

        let migration = zeroship_migrate_server::apply::runtime_role_provisioning_sql(
            app_id,
            "zs_migrator_fixture",
        )
        .expect("migration fixture role name");
        assert_eq!(
            migration.role_name(),
            role,
            "data-plane and migration-service role composers diverged"
        );

        for setup_sql in [
            crate::backend::pg_session_sql::tx_session_setup_sql(app_id)
                .expect("transaction setup role name"),
            crate::backend::pg_session_sql::autocommit_local_session_setup_sql(app_id)
                .expect("autocommit setup role name"),
        ] {
            assert!(
                setup_sql.starts_with(&format!("SET LOCAL ROLE {quoted_role};")),
                "data-plane setup SQL did not carry the shared role: {setup_sql}"
            );
        }

        let server_message = format!("role \"{role}\" does not exist");
        assert!(
            is_missing_per_app_session_role(
                &SqlState::INVALID_PARAMETER_VALUE,
                &server_message,
                app_id,
            ),
            "missing-role classifier did not recognize the shared role"
        );
    }

    #[test]
    fn missing_role_is_classified_from_the_measured_sqlstate() {
        use compio_postgres::error::SqlState;

        assert!(is_missing_per_app_session_role(
            &SqlState::INVALID_PARAMETER_VALUE,
            r#"role "app_nonexistent_role" does not exist"#,
            "nonexistent",
        ));
    }

    #[test]
    fn same_sqlstate_different_message_stays_unclassified() {
        use compio_postgres::error::SqlState;

        assert!(!is_missing_per_app_session_role(
            &SqlState::INVALID_PARAMETER_VALUE,
            r#"invalid value for parameter "statement_timeout": "yes""#,
            "nonexistent",
        ));
    }

    #[test]
    fn same_message_shape_different_sqlstate_stays_unclassified() {
        use compio_postgres::error::SqlState;

        assert!(!is_missing_per_app_session_role(
            &SqlState::UNDEFINED_TABLE,
            r#"role "app_nonexistent_role" does not exist"#,
            "nonexistent",
        ));
    }

    #[test]
    fn other_role_sqlstates_are_not_session_setup_missing_role() {
        use compio_postgres::error::SqlState;

        assert!(!is_missing_per_app_session_role(
            &SqlState::UNDEFINED_OBJECT,
            r#"role "app_x_role" does not exist"#,
            "x",
        ));
        assert!(!is_missing_per_app_session_role(
            &SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            r#"role "app_x_role" does not exist"#,
            "x",
        ));
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
