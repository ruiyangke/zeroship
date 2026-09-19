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

use crate::binding::DbBinding;
use zeroship_data_orm::error::{
    DbError, DenyReason, GRANT_REVOKED, GRANT_REVOKED_MESSAGE, SCHEMA_EPOCH_STALE,
    STALE_EPOCH_HINT, STALE_EPOCH_MESSAGE, SessionSetupDisposition, SessionSetupError,
};

/// Does this server error name the binding role the setup batch asked for?
///
/// This discriminator is called only where the caller knows it just issued
/// `SET LOCAL ROLE` for `binding`. Provenance is the primary guard; SQLSTATE
/// and the exact role name pin the measured server response.
///
/// The SQLSTATE alone is not enough. `SET LOCAL ROLE "missing"` reports **22023
/// `invalid_parameter_value`** (measured against postgres:16 -- `LOCATION:
/// call_string_check_hook, guc.c`), not 42704 `undefined_object`. 22023 is the
/// generic "bad GUC value" code, shared with `SET statement_timeout = 'yes'`,
/// so matching it alone would reclassify unrelated configuration failures as
/// creator-facing.
///
/// # Why the parameter is the binding
///
/// It has to recognise the SAME role the setup batch asked for, and that role
/// is composed once, on the binding. Taking the binding is what makes composing
/// a second spelling here impossible: the epoch is the last component of the
/// name, so a classifier that recomposed from the wrong epoch would stop
/// matching and a retired epoch would report a generic failure instead of
/// [`SCHEMA_EPOCH_STALE`].
fn is_missing_binding_role(
    code: &compio_postgres::error::SqlState,
    primary_message: &str,
    binding: &DbBinding,
) -> bool {
    use compio_postgres::error::SqlState;

    let Some(expected_role) = binding.session_role() else {
        return false;
    };
    code == &SqlState::INVALID_PARAMETER_VALUE
        && primary_message == format!("role \"{expected_role}\" does not exist")
}

/// Classify an error returned by the binding's `SET LOCAL ROLE` batch.
///
/// Call this only at the two session-setup sites, after connection acquisition
/// and transaction start have succeeded. All other PostgreSQL errors, including
/// pool connection failures, must use [`classify`].
///
/// The two refusals this splits are the whole runtime fence, and they differ in
/// what the caller should do:
///
/// - **42501** means the role exists and this login may not assume it, which is
///   a revoked binding. Terminal: the reconciler withdrew the membership and
///   left the role standing precisely so this stays distinguishable.
/// - **22023** means there is no such role. The epoch is part of the name, so
///   this is the shape this isolate was built against no longer being the shape
///   the database has - or a database not yet converged. Either is answered by
///   resolving the binding again.
///
/// `binding` must be the SAME value handed to
/// [`crate::backend::postgres::pg_session_sql::tx_session_setup_sql`] /
/// `autocommit_local_session_setup_sql` on the call this is classifying.
pub(crate) fn classify_pg_binding_session_setup(
    e: &compio_postgres::Error,
    binding: &DbBinding,
) -> SessionSetupError {
    if e.as_db_error()
        .is_some_and(|db| is_missing_binding_role(db.code(), db.message(), binding))
    {
        let msg = walk_pg_chain(e);
        tracing::warn!(
            error = %msg,
            "binding role missing; the schema epoch this build was resolved at is retired"
        );
        return SessionSetupError::new(
            SessionSetupDisposition::Preserve,
            DbError::config_hinted(SCHEMA_EPOCH_STALE, STALE_EPOCH_MESSAGE, STALE_EPOCH_HINT),
        );
    }

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

/// Test-only view of the contextual classifier's creator-facing error.
/// Production transaction code also consumes the private disposition.
#[cfg(test)]
pub fn classify_pg_binding_session_setup_for_tests(
    e: &compio_postgres::Error,
    binding: &DbBinding,
) -> DbError {
    classify_pg_binding_session_setup(e, binding).into_db_error()
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
    // `schema_epoch_stale` -- the missing binding-role classification.
    //
    // These drive `is_missing_binding_role` directly rather than the contextual
    // converter because `compio_postgres::Error` has no public constructor. The
    // real converter path against a live server is covered by
    // `crates/zeroship-data-v8/src/tests/postgres/roles.rs`; these pin the
    // discriminator's cheap edge cases.
    // -----------------------------------------------------------------

    use crate::binding::DbBinding;
    use zeroship_core::{BindingId, DatabaseId};

    fn creator_binding(epoch: u32) -> DbBinding {
        DbBinding::to_database(
            "app_classifier",
            "deploy_classifier",
            DatabaseId::mint(),
            BindingId::mint(),
            epoch,
        )
        .expect("the fixture ids compose a legal role name")
    }

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
        let binding = DbBinding::to_database("app_parity", "d", DatabaseId::mint(), edge.clone(), 4)
            .expect("the fixture ids compose");

        let granted = zeroship_migrate_server::datastore::cluster::binding_role(&edge, 4)
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

    /// The classifier recognises the role the batch asked for.
    ///
    /// CONTROL, differing in one variable: a message naming a DIFFERENT
    /// binding's role must be refused, so the passing arm measures the
    /// derivation rather than a predicate that says yes to any
    /// "role ... does not exist" text.
    #[test]
    fn a_missing_binding_role_is_classified_and_a_neighbours_is_not() {
        use compio_postgres::error::SqlState;

        let mine = creator_binding(1);
        let neighbour = creator_binding(1);
        let my_role = mine.session_role().expect("a creator binding narrows");
        let their_role = neighbour
            .session_role()
            .expect("a creator binding narrows");
        assert_ne!(my_role, their_role, "the control: two edges are two roles");

        assert!(is_missing_binding_role(
            &SqlState::INVALID_PARAMETER_VALUE,
            &format!("role \"{my_role}\" does not exist"),
            &mine,
        ));
        assert!(
            !is_missing_binding_role(
                &SqlState::INVALID_PARAMETER_VALUE,
                &format!("role \"{their_role}\" does not exist"),
                &mine,
            ),
            "a role this binding did not compose must not be classified"
        );
    }

    /// The epoch is part of what the classifier recognises.
    ///
    /// The epoch fence is the whole reason the role name carries one: an
    /// isolate built against a retired epoch must be told the shape moved, and
    /// a classifier blind to the epoch would report a generic failure instead.
    #[test]
    fn the_classifier_is_not_blind_to_the_epoch() {
        use compio_postgres::error::SqlState;

        let database = DatabaseId::mint();
        let edge = BindingId::mint();
        let at_one = DbBinding::to_database("app_e", "d", database.clone(), edge.clone(), 1)
            .expect("composes");
        let at_two =
            DbBinding::to_database("app_e", "d", database, edge, 2).expect("composes");
        let retired = at_one.session_role().expect("narrows");
        let live = at_two.session_role().expect("narrows");
        assert_ne!(retired, live, "the control: two epochs are two roles");

        assert!(
            is_missing_binding_role(
                &SqlState::INVALID_PARAMETER_VALUE,
                &format!("role \"{retired}\" does not exist"),
                &at_one,
            ),
            "the isolate at the retired epoch must be told its shape moved"
        );
        assert!(
            !is_missing_binding_role(
                &SqlState::INVALID_PARAMETER_VALUE,
                &format!("role \"{retired}\" does not exist"),
                &at_two,
            ),
            "a live isolate must not classify another epoch's missing role as its own"
        );
    }

    /// A binding that narrows to nothing composes no role, so it can recognise
    /// none. Its control is a creator binding on the same message shape.
    #[test]
    fn a_platform_binding_recognises_no_missing_role() {
        use compio_postgres::error::SqlState;

        let platform = DbBinding::platform(
            "platform",
            "fixture",
            crate::sql::SchemaName::new("zeroship").expect("fixture schema"),
        );
        let creator = creator_binding(1);
        let role = creator.session_role().expect("narrows");
        let message = format!("role \"{role}\" does not exist");

        assert!(!is_missing_binding_role(
            &SqlState::INVALID_PARAMETER_VALUE,
            &message,
            &platform,
        ));
        assert!(
            is_missing_binding_role(&SqlState::INVALID_PARAMETER_VALUE, &message, &creator),
            "control: the same message against the binding that composed the role"
        );
    }

    #[test]
    fn same_sqlstate_different_message_stays_unclassified() {
        use compio_postgres::error::SqlState;

        assert!(!is_missing_binding_role(
            &SqlState::INVALID_PARAMETER_VALUE,
            r#"invalid value for parameter "statement_timeout": "yes""#,
            &creator_binding(1),
        ));
    }

    #[test]
    fn same_message_shape_different_sqlstate_stays_unclassified() {
        use compio_postgres::error::SqlState;

        let binding = creator_binding(1);
        let role = binding.session_role().expect("narrows");
        for code in [
            SqlState::UNDEFINED_TABLE,
            SqlState::UNDEFINED_OBJECT,
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            // The revoked-binding refusal. It reaches `GRANT_REVOKED` through a
            // separate arm of the classifier and must never be read as a
            // retired epoch: one is terminal and the other is not.
            SqlState::INSUFFICIENT_PRIVILEGE,
        ] {
            assert!(
                !is_missing_binding_role(
                    &code,
                    &format!("role \"{role}\" does not exist"),
                    &binding,
                ),
                "SQLSTATE {} must not reach the retired-epoch classification",
                code.code()
            );
        }
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
