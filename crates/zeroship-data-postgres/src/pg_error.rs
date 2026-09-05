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
pub fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    let mut err = classify(&e);
    zeroship_data_core::error::prefix_message(&mut err, &format!("{context}: "));
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
    // `schema_not_provisioned` -- the missing per-app role classification.
    //
    // These drive `is_missing_per_app_session_role` directly rather than the
    // contextual converter because `compio_postgres::Error` has no public
    // constructor. The real converter path against a live server is covered by
    // `crates/zeroship-plugin-db/tests/missing_role.rs`; these pin the discriminator's cheap edge cases.
    // -----------------------------------------------------------------

    #[test]
    fn per_app_role_composers_match_across_services() {
        use compio_postgres::error::SqlState;

        let app_id = "role_parity";
        let role = zeroship_core::database_role::per_app_role_name(app_id)
            .expect("parity fixture role name");
        let quoted_role = zeroship_schema::query::quote_ident(&role);

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
            crate::pg_session_sql::tx_session_setup_sql(app_id)
                .expect("transaction setup role name"),
            crate::pg_session_sql::autocommit_local_session_setup_sql(app_id)
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

    /// The parity test above is blind to the divergence it appears to guard.
    ///
    /// `per_app_role_composers_match_across_services` passes the SINGLE string
    /// `"role_parity"` to both `runtime_role_provisioning_sql` -- whose parameter
    /// is named `schema` (crates/zeroship-migrate-server/src/apply.rs:1475) --
    /// and `tx_session_setup_sql`, whose parameter is named `app_id`
    /// (crates/zeroship-data-postgres/src/pg_session_sql.rs:36). It therefore
    /// proves the two composers AGREE GIVEN THE SAME INPUT, and proves nothing
    /// about the inputs agreeing. Both derive through
    /// `zeroship_core::database_role::per_app_role_name`, so given one string
    /// that agreement is close to a tautology.
    ///
    /// This test feeds them the two DIFFERENT values the app/database decoupling
    /// produces -- the physical schema becomes `db_<dbsid>` while the tenant
    /// stays the app uuid -- and pins the property the shipped test cannot see.
    ///
    /// Arm 1 DEMONSTRATES the blindness rather than asserting it: it reruns the
    /// shipped test's exact shape against each identity separately and shows
    /// both pass, so a same-input test is satisfied in the diverged world too.
    ///
    /// Arm 3 is the second-order damage, and it is why this matters beyond a
    /// failed transaction. `crates/zeroship-data-postgres/src/pg_autocommit.rs`
    /// calls `autocommit_local_session_setup_sql(app_id)` at :91 and
    /// `classify_pg_per_app_session_setup(&e, app_id)` at :93 -- ONE variable,
    /// two lines apart, both `&str`. When the flip makes the setup call take the
    /// schema, nothing in the type system objects to the classifier call keeping
    /// the tenant. The server then reports the schema-derived role while the
    /// classifier expects the tenant-derived one, the match fails, and an
    /// un-migrated database degrades from an actionable SCHEMA_NOT_PROVISIONED
    /// into a generic Failed.
    #[test]
    fn per_app_role_composers_agree_across_the_two_identities_not_one_string() {
        use zeroship_core::database_role::per_app_role_name;

        // The two identities the decoupling separates. One string is both today.
        const TENANT_APP_ID: &str = "0191e7a2-b3c4-4d5e-8f90-123456789abc";
        const SCHEMA_NAME: &str = "db_0191e7a2b3c44d5e8f90123456789abc";
        const MIGRATOR: &str = "zs_migrator_fixture";

        // ---- Arm 1: the blindness, demonstrated. ----
        // The shipped test's shape, run against each identity on its own. Both
        // hold, so that test stays GREEN through the divergence below.
        for single_input in [TENANT_APP_ID, SCHEMA_NAME] {
            let migration = zeroship_migrate_server::apply::runtime_role_provisioning_sql(
                single_input,
                MIGRATOR,
            )
            .expect("provisioning role name");
            let data_plane = per_app_role_name(single_input).expect("data-plane role name");
            assert_eq!(
                migration.role_name(),
                data_plane,
                "same-input parity holds for {single_input} in the diverged world too, \
                 which is exactly why a same-input test cannot detect the flip"
            );
        }

        // ---- Arm 2: the property the shipped test should have carried. ----
        // The migration service provisions the role from the SCHEMA it created.
        // The data plane's session setup asks for a role from the TENANT it was
        // dispatched for. `SET LOCAL ROLE` names an identifier, so the shipped
        // setup SQL must name the identifier that was actually created.
        //
        // Asserted at the SQL level rather than helper-to-helper: this is a fact
        // about the statement the data plane sends, and the two role spellings
        // are both carried in the failure message so one assertion diagnoses it.
        let provisioned =
            zeroship_migrate_server::apply::runtime_role_provisioning_sql(SCHEMA_NAME, MIGRATOR)
                .expect("provisioning role name");
        let requested = per_app_role_name(TENANT_APP_ID).expect("data-plane role name");
        let setup_sql =
            crate::pg_session_sql::tx_session_setup_sql(TENANT_APP_ID).expect("tx setup sql");
        assert!(
            setup_sql.starts_with(&format!(
                "SET LOCAL ROLE {};",
                zeroship_schema::query::quote_ident(provisioned.role_name())
            )),
            "session setup asks for a role derived from the tenant while the migration \
             service provisioned one derived from the schema, so SET LOCAL ROLE names an \
             identifier that was never created.\n  provisioned (from schema {SCHEMA_NAME}): \
             {}\n  requested (from tenant {TENANT_APP_ID}): {requested}",
            provisioned.role_name(),
        );
    }

    /// The classifier degradation, observed on its own.
    ///
    /// This is a SEPARATE test rather than a third arm of the one above,
    /// because a panic in that test would leave this consequence unreached and
    /// therefore unmeasured. They fail independently and must be seen to.
    ///
    /// `crates/zeroship-data-postgres/src/pg_autocommit.rs` builds the setup SQL
    /// at :91 and classifies its failure at :93 from ONE `&str` variable. When
    /// the flip gives the setup call the schema, nothing stops the classifier
    /// call from keeping the tenant -- both parameters are `&str` and both are
    /// spelled `app_id`. The server then names the schema-derived role that does
    /// not exist while the classifier expects the tenant-derived one, the exact
    /// string match at `is_missing_per_app_session_role` fails, and an
    /// un-migrated database stops reporting SCHEMA_NOT_PROVISIONED -- the one
    /// error that tells a creator to run `zeroship migrate`.
    #[test]
    fn missing_role_stays_classified_when_schema_and_tenant_diverge() {
        use compio_postgres::error::SqlState;

        const TENANT_APP_ID: &str = "0191e7a2-b3c4-4d5e-8f90-123456789abc";
        const SCHEMA_NAME: &str = "db_0191e7a2b3c44d5e8f90123456789abc";

        let provisioned = zeroship_migrate_server::apply::runtime_role_provisioning_sql(
            SCHEMA_NAME,
            "zs_migrator_fixture",
        )
        .expect("provisioning role name");

        // CONTROL, differing in one variable: with schema and tenant still the
        // same string, this classifier call succeeds. So the arm below fails
        // because the identities diverged, not because the fixture is malformed.
        let control = format!(
            "role \"{}\" does not exist",
            zeroship_core::database_role::per_app_role_name(SCHEMA_NAME).expect("control role")
        );
        assert!(
            is_missing_per_app_session_role(
                &SqlState::INVALID_PARAMETER_VALUE,
                &control,
                SCHEMA_NAME,
            ),
            "control: the classifier must recognise its own derivation: {control}"
        );

        // An un-migrated database. The session asked for the schema-derived role
        // and the server names it; the classifier's call site handed over the
        // tenant. It must still classify.
        let server_message = format!("role \"{}\" does not exist", provisioned.role_name());
        assert!(
            is_missing_per_app_session_role(
                &SqlState::INVALID_PARAMETER_VALUE,
                &server_message,
                TENANT_APP_ID,
            ),
            "the classifier derives its expected role from the tenant and the server named \
             the schema-derived role, so SCHEMA_NOT_PROVISIONED degrades to a generic \
             failure: {server_message}"
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
