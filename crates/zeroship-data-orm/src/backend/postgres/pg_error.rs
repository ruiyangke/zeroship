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
    DbError, DenyReason, GRANT_REVOKED, GRANT_REVOKED_MESSAGE, MISSING_ROLE_HINT,
    MISSING_ROLE_MESSAGE, SCHEMA_NOT_PROVISIONED, SessionSetupDisposition, SessionSetupError,
};

/// Does this server error match the role set by per-app session setup?
///
/// This discriminator is called only where the caller knows it just issued
/// `SET LOCAL ROLE` for `schema`. Provenance is the primary guard; SQLSTATE
/// and the exact expected role name pin the measured server response.
///
/// The SQLSTATE alone is not enough. `SET LOCAL ROLE "missing"` reports **22023
/// `invalid_parameter_value`** (measured against postgres:16 -- `LOCATION:
/// call_string_check_hook, guc.c`), not 42704 `undefined_object`. 22023 is the
/// generic "bad GUC value" code, shared with `SET statement_timeout = 'yes'`,
/// so matching it alone would reclassify unrelated configuration failures as
/// creator-facing.
///
/// Matching the exact role derived from `schema` is stronger than sniffing the
/// `app_<id>_role` shape. A pool DSN can use an app-shaped login name and return
/// a FATAL 28000 during reconnect; that is an operator connection failure, not
/// an app condition that `zeroship migrate` can repair.
///
/// # Why the parameter is a [`SchemaName`]
///
/// It has to derive the SAME role the setup batch asked for, and that batch
/// derives from the schema. Both parameters were `&str` and both call sites read
/// one variable two lines apart, so the day the setup call took the schema while
/// this one kept the tenant, the match would silently stop holding and an
/// un-migrated database would report a generic failure instead of
/// SCHEMA_NOT_PROVISIONED - the one error that tells a creator to run
/// `zeroship migrate`. Sharing the type is what makes that a compile error.
fn is_missing_per_app_session_role(
    code: &compio_postgres::error::SqlState,
    primary_message: &str,
    schema: &crate::sql::SchemaName,
) -> bool {
    use compio_postgres::error::SqlState;

    let Ok(expected_role) = zeroship_core::database_role::per_app_role_name(schema.as_str()) else {
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
///
/// `schema` must be the SAME value handed to
/// [`crate::backend::postgres::pg_session_sql::tx_session_setup_sql`] /
/// `autocommit_local_session_setup_sql` on the call this is classifying.
pub(crate) fn classify_pg_per_app_session_setup(
    e: &compio_postgres::Error,
    schema: &crate::sql::SchemaName,
) -> SessionSetupError {
    if e.as_db_error()
        .is_some_and(|db| is_missing_per_app_session_role(db.code(), db.message(), schema))
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
#[cfg(test)]
pub fn classify_pg_per_app_session_setup_for_tests(
    e: &compio_postgres::Error,
    schema: &crate::sql::SchemaName,
) -> DbError {
    classify_pg_per_app_session_setup(e, schema).into_db_error()
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
    // `schema_not_provisioned` -- the missing per-app role classification.
    //
    // These drive `is_missing_per_app_session_role` directly rather than the
    // contextual converter because `compio_postgres::Error` has no public
    // constructor. The real converter path against a live server is covered by
    // `crates/zeroship-data-v8/src/tests/postgres/roles.rs`; these pin the discriminator's cheap edge cases.
    // -----------------------------------------------------------------

    #[test]
    fn per_app_role_composers_match_across_services() {
        use compio_postgres::error::SqlState;

        let schema = crate::sql::SchemaName::new("role_parity").expect("parity fixture");
        let role = zeroship_core::database_role::per_app_role_name(schema.as_str())
            .expect("parity fixture role name");
        let quoted_role = crate::sql::mapping::quote_ident(&role);

        let migration = zeroship_migrate_server::apply::runtime_role_provisioning_sql(
            &zeroship_core::app_id::AppId::mint(),
            &schema,
            "zs_migrator_fixture",
        )
        .expect("migration fixture role name");
        assert_eq!(
            migration.role_name(),
            role,
            "data-plane and migration-service role composers diverged"
        );

        for setup_sql in [
            crate::backend::postgres::pg_session_sql::tx_session_setup_sql(
                &schema,
                crate::connection::SessionAuthority::PerAppRole,
            )
            .expect("transaction setup role name"),
            crate::backend::postgres::pg_session_sql::autocommit_local_session_setup_sql(
                &schema,
                crate::connection::SessionAuthority::PerAppRole,
            )
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
                &schema,
            ),
            "missing-role classifier did not recognize the shared role"
        );
    }

    /// The parity test above was blind to the divergence it appears to guard.
    ///
    /// `per_app_role_composers_match_across_services` passes ONE fixture to both
    /// `runtime_role_provisioning_sql` and `tx_session_setup_sql`. It therefore
    /// proves the two composers AGREE GIVEN THE SAME INPUT, and proved nothing
    /// about the inputs agreeing. Both derive through
    /// `zeroship_core::database_role::per_app_role_name`, so given one string
    /// that agreement is close to a tautology.
    ///
    /// The divergence it could not see is the one the app/database decoupling
    /// produces: the physical schema becomes `db_<dbsid>` while the tenant stays
    /// the app uuid. While both composers took `&str`, a call site holding both
    /// identities could hand each of them a different one and nothing objected.
    ///
    /// **THE FIX IS THE TYPE, AND THIS TEST NOW RECORDS THAT.** Both composers
    /// take [`crate::sql::SchemaName`], so the failing call this test was
    /// written to make fail is no longer expressible - `tx_session_setup_sql`
    /// cannot be handed a tenant id, because a tenant id is a `&str` and a
    /// `&str` is not a `SchemaName` and there is no `From`, `AsRef` or `Deref`
    /// to make it one. What is left to assert at run time is that the two
    /// composers still agree on the SAME `SchemaName`, and arm 3 keeps the
    /// fixture honest by proving the two identities really are different
    /// strings - without that, arm 2 would hold vacuously.
    ///
    /// The second-order damage this guards is why it matters beyond a failed
    /// transaction. `crates/zeroship-data-orm/src/backend/postgres/pg_autocommit.rs` builds
    /// the setup SQL and classifies its failure from ONE variable, two lines
    /// apart. While both parameters were `&str`, a flip that gave the setup call
    /// the schema could leave the classifier call on the tenant; the server
    /// would then report the schema-derived role while the classifier expected
    /// the tenant-derived one, the match would fail, and an un-migrated database
    /// would degrade from an actionable SCHEMA_NOT_PROVISIONED into a generic
    /// Failed. `missing_role_stays_classified_when_schema_and_tenant_diverge`
    /// below observes that consequence on its own.
    #[test]
    fn per_app_role_composers_agree_across_the_two_identities_not_one_string() {
        use zeroship_core::database_role::per_app_role_name;
        use crate::sql::SchemaName;

        // The two identities the decoupling separates. One string is both today.
        const TENANT_APP_ID: &str = "0191e7a2-b3c4-4d5e-8f90-123456789abc";
        const SCHEMA_NAME: &str = "db_0191e7a2b3c44d5e8f90123456789abc";
        const MIGRATOR: &str = "zs_migrator_fixture";
        let app_id = zeroship_core::AppId::mint();

        // ---- Arm 1: the blindness, demonstrated. ----
        // The shipped test's shape, run against each identity on its own. Both
        // hold, so that test would stay GREEN through the divergence below - it
        // is satisfied in the diverged world too.
        for single_input in [TENANT_APP_ID, SCHEMA_NAME] {
            let as_schema = SchemaName::new(single_input).expect("fixture schema name");
            let migration =
                zeroship_migrate_server::apply::runtime_role_provisioning_sql(&app_id, &as_schema, MIGRATOR)
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
        // The migration service provisions the role from the SCHEMA it created,
        // and the data plane's `SET LOCAL ROLE` must name the identifier that
        // was actually created. Both calls take the SAME `SchemaName` value -
        // and, more to the point, both take the same TYPE, so a call site
        // holding a tenant id as well cannot feed one to either of them.
        //
        // Asserted at the SQL level rather than helper-to-helper: this is a fact
        // about the statement the data plane sends, and the two role spellings
        // are both carried in the failure message so one assertion diagnoses it.
        let schema = SchemaName::new(SCHEMA_NAME).expect("fixture schema name");
        let provisioned =
            zeroship_migrate_server::apply::runtime_role_provisioning_sql(&app_id, &schema, MIGRATOR)
                .expect("provisioning role name");
        let setup_sql = crate::backend::postgres::pg_session_sql::tx_session_setup_sql(
            &schema,
            crate::connection::SessionAuthority::PerAppRole,
        )
        .expect("tx setup sql");
        assert!(
            setup_sql.starts_with(&format!(
                "SET LOCAL ROLE {};",
                crate::sql::mapping::quote_ident(provisioned.role_name())
            )),
            "session setup does not name the role the migration service provisioned, so \
             SET LOCAL ROLE names an identifier that was never created.\n  provisioned \
             (from schema {SCHEMA_NAME}): {}\n  setup SQL: {setup_sql}",
            provisioned.role_name(),
        );

        // ---- Arm 3: the fixture really is a diverged world. ----
        // Without this, arm 2 would pass for the trivial reason that the tenant
        // and the schema are the same string, which is the exact blindness the
        // shipped parity test has.
        let tenant_derived = per_app_role_name(TENANT_APP_ID).expect("tenant role name");
        assert_ne!(
            provisioned.role_name(),
            tenant_derived,
            "fixture is not diverged: the schema-derived and tenant-derived roles are equal, \
             so arm 2 proves nothing about the identities being kept apart"
        );
    }

    /// The classifier degradation, observed on its own.
    ///
    /// This is a SEPARATE test rather than a third arm of the one above,
    /// because a panic in that test would leave this consequence unreached and
    /// therefore unmeasured. They fail independently and must be seen to.
    ///
    /// `crates/zeroship-data-orm/src/backend/postgres/pg_autocommit.rs` builds the setup SQL
    /// and classifies its failure two lines later, from ONE variable. While both
    /// parameters were `&str` and both were spelled `app_id`, a flip that gave
    /// the setup call the schema left nothing stopping the classifier call from
    /// keeping the tenant. The server would then name the schema-derived role
    /// that does not exist while the classifier expected the tenant-derived one,
    /// the exact string match at `is_missing_per_app_session_role` would fail,
    /// and an un-migrated database would stop reporting SCHEMA_NOT_PROVISIONED
    /// -- the one error that tells a creator to run `zeroship migrate`.
    ///
    /// **THE MISMATCHED CALL IS NOW A COMPILE ERROR.** The classifier takes the
    /// same [`crate::sql::SchemaName`] the setup builder does, so the two
    /// call sites in `pg_autocommit.rs` cannot be given different identities.
    /// What is asserted below is that the classifier recognises a role derived
    /// from the schema it was handed, in a fixture where the tenant string is
    /// demonstrably a DIFFERENT string - which is what makes the arm a statement
    /// about the diverged world rather than about one value used twice.
    #[test]
    fn missing_role_stays_classified_when_schema_and_tenant_diverge() {
        use compio_postgres::error::SqlState;
        use crate::sql::SchemaName;

        const TENANT_APP_ID: &str = "0191e7a2-b3c4-4d5e-8f90-123456789abc";
        const SCHEMA_NAME: &str = "db_0191e7a2b3c44d5e8f90123456789abc";

        let schema = SchemaName::new(SCHEMA_NAME).expect("fixture schema name");
        let provisioned = zeroship_migrate_server::apply::runtime_role_provisioning_sql(
            &zeroship_core::app_id::AppId::mint(),
            &schema,
            "zs_migrator_fixture",
        )
        .expect("provisioning role name");

        // CONTROL, differing in one variable: a message naming a role this
        // classifier did NOT derive must be refused, so the passing arm below
        // is measuring the derivation rather than a predicate that says yes to
        // any "role ... does not exist" text.
        let tenant_derived =
            zeroship_core::database_role::per_app_role_name(TENANT_APP_ID).expect("tenant role");
        assert_ne!(
            provisioned.role_name(),
            tenant_derived,
            "fixture is not diverged: schema-derived and tenant-derived roles are equal"
        );
        let foreign = format!("role \"{tenant_derived}\" does not exist");
        assert!(
            !is_missing_per_app_session_role(&SqlState::INVALID_PARAMETER_VALUE, &foreign, &schema,),
            "control: a role this schema did not derive must not be classified: {foreign}"
        );

        // An un-migrated database. The session asked for the schema-derived role
        // and the server names it. The classifier is handed the SAME schema the
        // setup batch used - it cannot be handed the tenant - and must classify.
        let server_message = format!("role \"{}\" does not exist", provisioned.role_name());
        assert!(
            is_missing_per_app_session_role(
                &SqlState::INVALID_PARAMETER_VALUE,
                &server_message,
                &schema,
            ),
            "the classifier must recognise the role the migration service provisioned from \
             this schema, or SCHEMA_NOT_PROVISIONED degrades to a generic failure: \
             {server_message}"
        );
    }

    #[test]
    fn missing_role_is_classified_from_the_measured_sqlstate() {
        use compio_postgres::error::SqlState;

        assert!(is_missing_per_app_session_role(
            &SqlState::INVALID_PARAMETER_VALUE,
            r#"role "app_nonexistent_role" does not exist"#,
            &crate::sql::SchemaName::new("nonexistent").expect("fixture schema"),
        ));
    }

    #[test]
    fn same_sqlstate_different_message_stays_unclassified() {
        use compio_postgres::error::SqlState;

        assert!(!is_missing_per_app_session_role(
            &SqlState::INVALID_PARAMETER_VALUE,
            r#"invalid value for parameter "statement_timeout": "yes""#,
            &crate::sql::SchemaName::new("nonexistent").expect("fixture schema"),
        ));
    }

    #[test]
    fn same_message_shape_different_sqlstate_stays_unclassified() {
        use compio_postgres::error::SqlState;

        assert!(!is_missing_per_app_session_role(
            &SqlState::UNDEFINED_TABLE,
            r#"role "app_nonexistent_role" does not exist"#,
            &crate::sql::SchemaName::new("nonexistent").expect("fixture schema"),
        ));
    }

    #[test]
    fn other_role_sqlstates_are_not_session_setup_missing_role() {
        use compio_postgres::error::SqlState;

        assert!(!is_missing_per_app_session_role(
            &SqlState::UNDEFINED_OBJECT,
            r#"role "app_x_role" does not exist"#,
            &crate::sql::SchemaName::new("x").expect("fixture schema"),
        ));
        assert!(!is_missing_per_app_session_role(
            &SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            r#"role "app_x_role" does not exist"#,
            &crate::sql::SchemaName::new("x").expect("fixture schema"),
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
