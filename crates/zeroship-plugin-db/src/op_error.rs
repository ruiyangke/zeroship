//! The adapter's own error translator: `DbError` -> `OpError`.
//!
//! This is the OUTBOUND half of the data plane's error hierarchy and it lives
//! here rather than in `zeroship-data-core` on purpose. `OpError` is
//! `zeroship-runtime`'s type - a DELIVERY MECHANISM - and a domain type that
//! names one is a dependency pointing outward. The core owns `DbError`; each
//! vendor crate translates its driver's error INTO it; and this file, in the
//! only crate that is allowed to link V8 and the runtime, lowers it back out to
//! the op boundary.
//!
//! It is an extension trait rather than a free function so that the call sites
//! keep reading `err.to_op_error()`. Rust requires the trait in scope, which is
//! the point: every file that lowers a domain error to the runtime says so in
//! its imports.

use zeroship_data_core::error::DbError;
use zeroship_runtime::state::OpError;

/// Lower a domain error to the runtime's op-boundary error.
pub trait ToOpError {
    /// Stamp the canonical `.code` for the variant onto an `OpError`.
    fn to_op_error(self) -> OpError;
}

impl ToOpError for DbError {
    /// Stamp this `DbError` onto an `OpError` with the canonical
    /// `.code` for the variant. The runtime materialises a JS Error
    /// with `e.code` (and `e.hint` when present) — the SDK reads it
    /// directly.
    fn to_op_error(self) -> OpError {
        match self {
            DbError::SchemaRefused {
                code,
                envelope_json,
            } => {
                // SchemaRefused carries a JSON envelope the SDK parses
                // verbatim. We stamp the static `code` so SDK callers
                // can branch on `e.code === "validation_refused"` without
                // resorting to JSON.parse(e.message). The envelope JSON
                // stays the message body so existing callers that
                // serde_json::from_str(&err.to_string()) continue to
                // parse the envelope correctly.
                OpError::coded(code, envelope_json, None::<String>)
            }
            DbError::ValidationFailed {
                code,
                message,
                hint,
            } => OpError::coded(code, message, hint),
            DbError::UniqueViolation { message } => {
                OpError::coded("unique_violation", message, None::<String>)
            }
            DbError::FkViolation { message } => {
                OpError::coded("fk_violation", message, None::<String>)
            }
            DbError::NotNullViolation { message } => {
                OpError::coded("not_null_violation", message, None::<String>)
            }
            DbError::CheckViolation { message } => {
                OpError::coded("check_violation", message, None::<String>)
            }
            DbError::Serialization { message } => OpError::coded(
                "serialization_failure",
                message,
                Some(
                    "retry the transaction; Postgres SSI / deadlock detector aborted it"
                        .to_string(),
                ),
            ),
            DbError::LockContention { message } => OpError::coded(
                "lock_not_available",
                message,
                Some(
                    "Retry after a short backoff; another worker holds the lock briefly."
                        .to_string(),
                ),
            ),
            DbError::Transient { message } => OpError::coded(
                "transient",
                message,
                Some("transient backend failure; retry after a short backoff".to_string()),
            ),
            DbError::Configuration {
                code,
                message,
                hint,
            } => OpError::coded(code, message, hint),
            DbError::PermissionDenied { code, message } => {
                OpError::coded_with_status(code, message, None::<String>, 403)
            }
            DbError::Coded {
                code,
                message,
                hint,
            } => OpError::coded(code, message, hint),
            DbError::AccessDenied { code } => OpError::coded(
                code,
                "platform-internal capability is not reachable from app code",
                None::<String>,
            ),
            DbError::Internal { message } => OpError::coded("internal", message, None::<String>),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_data_core::error::*;

    #[test]
    fn missing_role_message_names_the_command_and_leaks_no_identity() {
        // THE ACCEPTANCE BAR. A creator who skipped the migrate step must
        // learn what to run from the RESPONSE, not from a worker log they
        // cannot see. The remediation must be in `message`: `hint` is
        // populated and then dropped -- `build_verbose_error_body` never
        // emits it.
        let err = DbError::config_hinted(
            SCHEMA_NOT_PROVISIONED,
            MISSING_ROLE_MESSAGE,
            MISSING_ROLE_HINT,
        );
        let op = err.to_op_error();
        match &op.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint, .. } => {
                assert_eq!(code, SCHEMA_NOT_PROVISIONED);
                assert!(hint.is_some(), "hint is set for direct env.db callers");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
        assert!(
            op.message.contains("zeroship migrate"),
            "the response must name the command that fixes it: {}",
            op.message
        );
        // Platform-authored and fixed: no server text, no role name, no
        // app id, and nothing interpolated at all.
        assert!(
            !op.message.contains("_role"),
            "no role name: {}",
            op.message
        );
        assert!(
            !op.message.contains("ERROR:"),
            "no server text: {}",
            op.message
        );
        assert!(op.message.is_ascii(), "ASCII only: {}", op.message);
    }

    #[test]
    fn prefix_message_leaves_the_provisioning_message_alone() {
        // `exec.rs` adds "db: per-app session setup: " to ordinary setup
        // failures. `prefix_message` skips `Configuration`, so the
        // creator-facing string stays clean. If that skip is ever removed,
        // operator-only setup context would leak through a public code.
        let mut err = DbError::config_hinted(
            SCHEMA_NOT_PROVISIONED,
            MISSING_ROLE_MESSAGE,
            MISSING_ROLE_HINT,
        );
        prefix_message(&mut err, "db: per-app session setup: ");
        assert_eq!(err.message_str(), MISSING_ROLE_MESSAGE);
    }

    #[test]
    fn validation_failed_stamps_code() {
        let e = DbError::validation("filter_nesting_too_deep", "max 16 levels").to_op_error();
        match &e.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                assert_eq!(code, "filter_nesting_too_deep");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
        assert_eq!(e.message, "max 16 levels");
    }

    #[test]
    fn configuration_stamps_code() {
        let e = DbError::config("not_configured", "db url missing").to_op_error();
        match &e.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint, .. } => {
                assert_eq!(code, "not_configured");
                assert!(hint.is_none());
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    #[test]
    fn schema_refused_into_string_returns_envelope() {
        let envelope = r#"{"code":"validation_refused","violations":[]}"#;
        let s = DbError::SchemaRefused {
            code: "validation_refused",
            envelope_json: envelope.to_string(),
        }
        .into_string();
        assert_eq!(s, envelope);
    }

    /// `SchemaRefused` must stamp `.code = "validation_refused"` on the
    /// JS exception so SDK callers can branch on `e.code` without
    /// JSON.parse'ing the message. The message body must still be the
    /// raw envelope JSON so existing callers that
    /// `serde_json::from_str(&err.to_string())` continue to parse it.
    #[test]
    fn schema_refused_stamps_code_and_preserves_envelope_as_message() {
        let envelope = r#"{"code":"validation_refused","violations":[]}"#;
        let op = DbError::SchemaRefused {
            code: "validation_refused",
            envelope_json: envelope.to_string(),
        }
        .to_op_error();
        match &op.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint, .. } => {
                assert_eq!(code, "validation_refused");
                assert!(hint.is_none());
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
        // Message must be the raw envelope so existing JSON.parse callers work.
        assert_eq!(op.message, envelope);
        // Verify it round-trips as valid JSON (the serde_json::from_str path).
        serde_json::from_str::<serde_json::Value>(&op.message)
            .expect("message must be valid JSON envelope");
    }

    #[test]
    fn coded_passthrough_preserves_code() {
        let e = DbError::Coded {
            code: "migration_already_running".to_string(),
            message: "x".to_string(),
            hint: Some("y".to_string()),
        }
        .to_op_error();
        match &e.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint, .. } => {
                assert_eq!(code, "migration_already_running");
                assert_eq!(hint.as_deref(), Some("y"));
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    #[test]
    fn permission_denied_carries_its_terminal_http_status() {
        let error = DbError::PermissionDenied {
            code: GRANT_REVOKED,
            message: GRANT_REVOKED_MESSAGE,
        }
        .to_op_error();
        match &error.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, status, .. } => {
                assert_eq!(code, GRANT_REVOKED);
                assert_eq!(*status, Some(403));
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
        assert_eq!(error.message, GRANT_REVOKED_MESSAGE);
    }

    /// Sweep the SQL-violation variants (the four 23xxx codes plus
    /// Serialization, LockContention, Transient, Internal) — each must
    /// stamp the canonical wire `code` that the SDK branches on. The
    /// pre-existing four tests cover the *bespoke* paths (SchemaRefused
    /// envelope, ValidationFailed, Configuration, Coded passthrough);
    /// this sweep guards the much larger constant-table set against a
    /// rename that would silently break SDK error handling.
    #[test]
    fn sql_violation_variants_stamp_canonical_codes() {
        fn op_code(e: DbError) -> String {
            match e.to_op_error().kind {
                zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => code,
                other => panic!("expected CodedError, got {other:?}"),
            }
        }
        let cases = [
            (
                DbError::UniqueViolation { message: "".into() },
                "unique_violation",
            ),
            (DbError::FkViolation { message: "".into() }, "fk_violation"),
            (
                DbError::NotNullViolation { message: "".into() },
                "not_null_violation",
            ),
            (
                DbError::CheckViolation { message: "".into() },
                "check_violation",
            ),
            (
                DbError::Serialization { message: "".into() },
                "serialization_failure",
            ),
            (
                DbError::LockContention { message: "".into() },
                "lock_not_available",
            ),
            (DbError::Transient { message: "".into() }, "transient"),
            (DbError::Internal { message: "".into() }, "internal"),
        ];
        for (variant, expected_code) in cases {
            let got = op_code(variant);
            assert_eq!(got, expected_code, "wrong wire code for variant");
        }
    }

    /// Retryable variants (Serialization, Transient, LockContention) must
    /// carry a human-facing `hint` so the SDK can surface recovery advice
    /// without re-deriving it. Non-retryable ones (UniqueViolation, etc.)
    /// must NOT — the SDK treats a hinted error as recoverable advice.
    #[test]
    fn retryable_variants_carry_hint() {
        fn op_hint(e: DbError) -> Option<String> {
            match e.to_op_error().kind {
                zeroship_runtime::state::OpErrorKind::CodedError { hint, .. } => hint,
                other => panic!("expected CodedError, got {other:?}"),
            }
        }
        assert!(op_hint(DbError::Serialization {
            message: "x".into()
        })
        .is_some());
        assert!(op_hint(DbError::Transient {
            message: "x".into()
        })
        .is_some());
        // LockContention is retriable — must also carry a hint.
        assert!(op_hint(DbError::LockContention {
            message: "x".into()
        })
        .is_some());
        // Non-retryable violations must not advise a retry.
        assert!(op_hint(DbError::UniqueViolation {
            message: "x".into()
        })
        .is_none());
        assert!(op_hint(DbError::FkViolation {
            message: "x".into()
        })
        .is_none());
        assert!(op_hint(DbError::Internal {
            message: "x".into()
        })
        .is_none());
    }

    /// The helper returns the first element of a non-empty slice. The
    /// test uses `Vec<i64>` because the helper is deliberately generic over
    /// the backend row representation.
    #[test]
    fn first_row_or_internal_returns_first_on_non_empty() {
        let rows: Vec<i64> = vec![7, 8, 9];
        let got = first_row_or_internal(&rows, "test op").expect("non-empty");
        assert_eq!(*got, 7);
    }

    /// On an empty slice the helper must produce `DbError::Internal`
    /// whose message names the operation. The audit-id=0 regression is
    /// the canonical site this contract protects: substring matching
    /// against the op name is how the in-tree regression test in
    /// `audit.rs` verifies the contract.
    #[test]
    fn first_row_or_internal_returns_internal_err_on_empty() {
        let rows: Vec<i64> = vec![];
        let err =
            first_row_or_internal(&rows, "audit: INSERT").expect_err("empty slice must error");
        match err {
            DbError::Internal { message } => {
                assert!(
                    message.contains("audit: INSERT"),
                    "message must name the op, got: {message}"
                );
                assert!(
                    message.contains("returned no row"),
                    "message must carry the canonical suffix, got: {message}"
                );
            }
            other => panic!("expected DbError::Internal, got {other:?}"),
        }
    }

    /// `prefix_message` must leave the variant intact so the SQLSTATE
    /// classification still drives the wire `.code` at the V8 boundary;
    /// it only prepends the context phrase to the human body. Without
    /// this guarantee, prefixing in any of the 6 call-site modules
    /// would silently re-flatten everything to `Internal` and break the
    /// SDK's `.code`-branching contract.
    #[test]
    fn prefix_message_preserves_variant_and_code() {
        let cases = [
            (
                DbError::UniqueViolation {
                    message: "boom".into(),
                },
                "unique_violation",
            ),
            (
                DbError::FkViolation {
                    message: "boom".into(),
                },
                "fk_violation",
            ),
            (
                DbError::NotNullViolation {
                    message: "boom".into(),
                },
                "not_null_violation",
            ),
            (
                DbError::CheckViolation {
                    message: "boom".into(),
                },
                "check_violation",
            ),
            (
                DbError::Serialization {
                    message: "boom".into(),
                },
                "serialization_failure",
            ),
            (
                DbError::LockContention {
                    message: "boom".into(),
                },
                "lock_not_available",
            ),
            (
                DbError::Transient {
                    message: "boom".into(),
                },
                "transient",
            ),
            (
                DbError::Internal {
                    message: "boom".into(),
                },
                "internal",
            ),
        ];
        for (mut variant, expected_code) in cases {
            prefix_message(&mut variant, "audit: ctx: ");
            // Body must have been prefixed AND keep the original tail.
            let body = variant.to_string();
            assert!(
                body.starts_with("audit: ctx: "),
                "missing prefix in body: {variant:?}"
            );
            assert!(
                body.ends_with("boom"),
                "original body lost after prefixing: {variant:?}"
            );
            // Variant -> wire code unchanged.
            let op = variant.to_op_error();
            match op.kind {
                zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                    assert_eq!(code, expected_code, "wire code drifted for variant");
                }
                other => panic!("expected CodedError, got {other:?}"),
            }
        }
    }

    /// `prefix_message` is a no-op for the structured variants whose
    /// `.code` and message body are part of the SDK contract
    /// (Configuration, Coded, ValidationFailed, SchemaRefused). Their
    /// messages already carry their semantic (the `SchemaRefused`
    /// envelope is parsed verbatim by the SDK); prefixing would distort
    /// the wire payload.
    #[test]
    fn prefix_message_leaves_structured_variants_alone() {
        // Configuration — message body must be untouched, code preserved.
        let mut cfg = DbError::Configuration {
            code: "wal_level_not_logical",
            message: "needs logical".into(),
            hint: None,
        };
        prefix_message(&mut cfg, "replication: ctx: ");
        assert_eq!(cfg.to_string(), "needs logical");
        match cfg.to_op_error().kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                assert_eq!(code, "wal_level_not_logical");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }

        // ValidationFailed — message body + code untouched.
        let mut val = DbError::ValidationFailed {
            code: "invalid_app_id",
            message: "must be [A-Za-z0-9_]".into(),
            hint: None,
        };
        prefix_message(&mut val, "diff: ctx: ");
        assert_eq!(val.to_string(), "must be [A-Za-z0-9_]");
        match val.to_op_error().kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                assert_eq!(code, "invalid_app_id");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }

        // Coded — message body + dynamic code untouched.
        let mut coded = DbError::Coded {
            code: "migration_already_running".to_string(),
            message: "another worker holds the lock".into(),
            hint: Some("retry later".into()),
        };
        prefix_message(&mut coded, "audit: ctx: ");
        assert_eq!(coded.to_string(), "another worker holds the lock");
        match coded.to_op_error().kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint, .. } => {
                assert_eq!(code, "migration_already_running");
                assert_eq!(hint.as_deref(), Some("retry later"));
            }
            other => panic!("expected CodedError, got {other:?}"),
        }

        // SchemaRefused — envelope JSON body is the wire format; must
        // round-trip unchanged through `prefix_message`.
        let envelope = r#"{"code":"validation_refused","violations":[]}"#;
        let mut refused = DbError::SchemaRefused {
            code: "validation_refused",
            envelope_json: envelope.to_string(),
        };
        prefix_message(&mut refused, "diff: ctx: ");
        assert_eq!(refused.to_string(), envelope);
        match refused.to_op_error().kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, .. } => {
                assert_eq!(code, "validation_refused");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    /// `From<QueryError>` collapses the builder's four error kinds
    /// onto a `ValidationFailed` with a stable static code the SDK
    /// branches on. Each kind must map to a distinct code.
    ///
    /// The three legacy kinds carry no hint; the new
    /// `ReservedSystemFieldName` variant carries a fixed hint listing
    /// all seven system fields (covered separately by
    /// `from_query_error_reserved_system_field_carries_hint`).
    #[test]
    fn from_query_error_assigns_distinct_codes() {
        let cases = [
            (
                crate::compile::QueryError::InvalidFilter("bad".into()),
                "invalid_filter",
            ),
            (
                crate::compile::QueryError::InvalidCollection("bad".into()),
                "invalid_collection",
            ),
            (
                crate::compile::QueryError::InvalidIdent("bad".into()),
                "invalid_identifier",
            ),
        ];
        for (qe, expected_code) in cases {
            let db = DbError::from(qe);
            match db {
                DbError::ValidationFailed { code, hint, .. } => {
                    assert_eq!(code, expected_code);
                    assert!(hint.is_none(), "builder errors carry no hint");
                }
                other => panic!("expected ValidationFailed, got {other:?}"),
            }
        }
    }

    /// `ReservedSystemFieldName` maps to a distinct
    /// `reserved_system_field_name` code and carries a hint enumerating
    /// the seven system fields.
    #[test]
    fn from_query_error_reserved_system_field_carries_hint() {
        let qe = crate::compile::QueryError::ReservedSystemFieldName(
            "Field name 'id' is reserved".into(),
        );
        let db = DbError::from(qe);
        match db {
            DbError::ValidationFailed { code, hint, .. } => {
                assert_eq!(code, "reserved_system_field_name");
                let hint = hint.expect("reserved-system-field error must carry a hint");
                for name in [
                    "id",
                    "created_at",
                    "updated_at",
                    "created_by",
                    "updated_by",
                    "version",
                    "deleted_at",
                ] {
                    assert!(
                        hint.contains(name),
                        "hint must list system field {name:?}; got: {hint}"
                    );
                }
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// Relocated from `query.rs`'s test module
    /// (which moved to the leaf crate `zeroship-data-query-builder`, where `DbError`
    /// is not nameable). Pins the end-to-end lift: the schema-crate
    /// validator `validate_field_name_for_declaration` rejects a reserved
    /// system field, and `From<QueryError> for DbError` (which lives here)
    /// stamps `code = "reserved_system_field_name"` + a hint listing all 7
    /// system fields. Behaviour-identical to the pre-extraction test.
    #[test]
    fn system_field_reservation_error_carries_correct_code() {
        let err = crate::compile::validate_field_name_for_declaration("id").unwrap_err();
        let db_err = DbError::from(err);
        match db_err {
            DbError::ValidationFailed { code, hint, .. } => {
                assert_eq!(code, "reserved_system_field_name");
                let hint = hint.expect("reservation hint required for SDK remediation");
                for name in crate::compile::SYSTEM_FIELD_NAMES {
                    assert!(
                        hint.contains(name),
                        "reservation hint must list all 7 system fields; missing {name:?}"
                    );
                }
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    // ---- New typed-error helpers ------------------------

    /// `ImmutableSystemField` maps to the `immutable_system_field`
    /// code with a hint listing the three write-once names.
    #[test]
    fn from_query_error_immutable_system_field_carries_hint() {
        let qe = crate::compile::QueryError::ImmutableSystemField(
            "UPDATE patch attempted to overwrite immutable system field `id`".into(),
        );
        let db = DbError::from(qe);
        match db {
            DbError::ValidationFailed { code, hint, .. } => {
                assert_eq!(code, "immutable_system_field");
                let hint = hint.expect("immutable-system-field must carry a hint");
                for name in ["id", "created_at", "created_by"] {
                    assert!(
                        hint.contains(name),
                        "hint must list immutable field {name:?}; got: {hint}"
                    );
                }
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// `DbError::version_mismatch` stamps the canonical
    /// `version_mismatch` code; carries a hint advising re-read +
    /// retry.
    #[test]
    fn version_mismatch_stamps_canonical_code_and_hint() {
        let e = DbError::version_mismatch("posts", Some("post_x"), 5).to_op_error();
        match &e.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint, .. } => {
                assert_eq!(code, "version_mismatch");
                let h = hint.as_deref().expect("must carry a retry hint");
                assert!(h.to_lowercase().contains("retry"), "hint: {h}");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
        // Message body includes the collection + id + expected version
        // so SDK consumers don't have to reconstruct the context.
        assert!(e.message.contains("posts"), "message: {}", e.message);
        assert!(e.message.contains("post_x"), "message: {}", e.message);
        assert!(e.message.contains("5"), "message: {}", e.message);
    }

    /// `DbError::version_mismatch` without a row id omits the id
    /// segment from the message (used for multi-row UPDATEs whose
    /// filter doesn't carry id).
    #[test]
    fn version_mismatch_message_handles_missing_id() {
        let e = DbError::version_mismatch("posts", None, 5).to_op_error();
        assert!(e.message.contains("posts"));
        assert!(e.message.contains("5"));
    }

    /// `DbError::multi_row_version_filter_unsupported` stamps the
    /// canonical code; carries a remediation hint pointing at the
    /// per-id loop.
    #[test]
    fn multi_row_version_filter_unsupported_stamps_canonical_code() {
        let e = DbError::multi_row_version_filter_unsupported("posts").to_op_error();
        match &e.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint, .. } => {
                assert_eq!(code, "multi_row_version_filter_unsupported");
                assert!(hint.is_some(), "must carry a remediation hint");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    #[test]
    fn version_filter_must_be_top_level_stamps_canonical_code() {
        let e = DbError::version_filter_must_be_top_level("posts").to_op_error();
        match &e.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint, .. } => {
                assert_eq!(code, "version_filter_must_be_top_level");
                assert!(hint.is_some(), "must carry a remediation hint");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }
}
