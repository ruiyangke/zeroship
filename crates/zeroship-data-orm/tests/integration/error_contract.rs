use zeroship_data_orm::error::DbError;

#[test]
fn native_errors_expose_stable_codes_without_rendering_messages() {
    for (error, expected) in [
        (
            DbError::UniqueViolation {
                message: "backend detail".into(),
            },
            "unique_violation",
        ),
        (
            DbError::CheckViolation {
                message: "backend detail".into(),
            },
            "check_violation",
        ),
        (
            DbError::FkViolation {
                message: "backend detail".into(),
            },
            "fk_violation",
        ),
        (
            DbError::NotNullViolation {
                message: "backend detail".into(),
            },
            "not_null_violation",
        ),
        (
            DbError::Serialization {
                message: "backend detail".into(),
            },
            "serialization_failure",
        ),
        (
            DbError::LockContention {
                message: "backend detail".into(),
            },
            "lock_not_available",
        ),
        (
            DbError::Transient {
                message: "backend detail".into(),
            },
            "transient",
        ),
        (DbError::internal("backend detail"), "internal"),
        (
            DbError::validation("invalid_input", "backend detail"),
            "invalid_input",
        ),
        (
            DbError::config("missing_configuration", "backend detail"),
            "missing_configuration",
        ),
        (
            DbError::PermissionDenied {
                code: "denied",
                message: "backend detail",
            },
            "denied",
        ),
        (
            DbError::AccessDenied {
                code: "access_denied",
            },
            "access_denied",
        ),
        (
            DbError::SchemaRefused {
                code: "schema_refused",
                envelope_json: "{}".into(),
            },
            "schema_refused",
        ),
    ] {
        assert_eq!(error.code(), expected);
    }
    let code = String::from("caller_failure");
    let buffer = code.as_ptr();
    let error = DbError::Coded {
        code,
        message: "backend detail".into(),
        hint: None,
    };
    assert_eq!(error.code(), "caller_failure");
    assert_eq!(error.code().as_ptr(), buffer);
}
