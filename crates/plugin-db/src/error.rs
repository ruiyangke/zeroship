//! Typed error classification for `plugin-db`.
//!
//! Every fallible internal helper returns `Result<_, DbError>`. At the
//! V8 boundary the dispatcher calls [`DbError::to_op_error`] to
//! materialise an [`OpError`] whose `.code` is stamped from the variant
//! — the SDK can then branch on `err.code` instead of substring-matching
//! opaque messages.
//!
//! `Result<_, String>` is not yet fully eliminated below the JS
//! boundary. Known remaining sites (see backlog [I28]): replication.rs
//! (~7 sites), the `auth/*` bootstrap helpers (~15 sites), parts of
//! `diff.rs`, plus the `validate` stage in
//! `crate::orchestrator::register_model` whose `Err` is the
//! `validation_refused` JSON envelope (a documented SDK wire contract).
//! `run_pipeline` and the orchestrator dispatchers wrap those strings
//! in typed [`DbError`] variants at the boundary, so the typed-error
//! invariant still holds at every JS-visible surface — but the SDK
//! loses `.code` discrimination on the wrapped paths and a mechanical
//! sweep is pending.
//!
//! The wire format JS sees is unchanged: still a JS `Error` with
//! `message` + `code` (+ `hint` when present). All this layer does is
//! discipline the *origin* of the code so every native throw carries
//! one.
//!
//! ## When to use which variant
//!
//! | Variant | Cause | Example |
//! |---|---|---|
//! | [`DbError::SchemaRefused`] | DDL deploy rejected before any rows touched | `validation_refused` envelope |
//! | [`DbError::ValidationFailed`] | User-supplied input failed a guardrail | bad isolation level, filter too deep |
//! | [`DbError::UniqueViolation`] | Postgres 23505 | INSERT into UNIQUE index |
//! | [`DbError::FkViolation`] | Postgres 23503 | INSERT references missing parent |
//! | [`DbError::NotNullViolation`] | Postgres 23502 | INSERT/UPDATE without required col |
//! | [`DbError::CheckViolation`] | Postgres 23514 | row failed CHECK constraint |
//! | [`DbError::Serialization`] | Postgres 40001 | SSI conflict in REPEATABLE READ / SERIALIZABLE |
//! | [`DbError::LockContention`] | Postgres 55P03 / lock-not-available | `SELECT … FOR UPDATE NOWAIT` |
//! | [`DbError::Transient`] | Postgres class 08, deadlock, out-of-memory | connection drop, 40P01 |
//! | [`DbError::Configuration`] | Plugin mis-configured | `DB_URL` not set |
//! | [`DbError::Coded`] | Pre-typed code from another subsystem | migrations.rs `migration_*` codes |
//! | [`DbError::Internal`] | Anything else; logged but stamped `internal` | a `JSON.stringify` that lost a column |

use zeroship_runtime::state::OpError;

/// Classified error origin for every fallible `plugin-db` helper.
///
/// Construct via the variant directly, or via [`DbError::from_pg`] for
/// Postgres-shaped errors (preserves SQLSTATE + walks the source chain
/// so the cause reaches the JS console).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DbError {
    /// DDL deploy refused before any rows changed — typically a
    /// `validation_refused` envelope from
    /// `orchestrator::register_model`. The full JSON envelope is in
    /// `envelope_json`; the variant exists so callers know the wire
    /// payload is *already* a JSON object the SDK consumes verbatim.
    SchemaRefused {
        code: &'static str,
        envelope_json: String,
    },

    /// User-supplied input failed a static guardrail (bad isolation
    /// level, malformed filter, validate-only collection name failure,
    /// nesting cap, …). Distinguished from
    /// [`DbError::UniqueViolation`] by *who* refused — `ValidationFailed`
    /// is the plugin's own pre-flight, not Postgres.
    ValidationFailed {
        code: &'static str,
        message: String,
        hint: Option<String>,
    },

    /// Postgres 23505 — unique constraint violation. `collection` is
    /// best-effort (parsed from the error detail when available).
    UniqueViolation {
        message: String,
    },

    /// Postgres 23503 — foreign key constraint violation.
    FkViolation {
        message: String,
    },

    /// Postgres 23502 — NOT NULL violation.
    NotNullViolation {
        message: String,
    },

    /// Postgres 23514 — CHECK constraint violation.
    CheckViolation {
        message: String,
    },

    /// Postgres 40001 / 40P01 — serialization failure or deadlock.
    /// SDK callers can retry these via the standard exponential
    /// backoff loop.
    Serialization {
        message: String,
    },

    /// Postgres 55P03 / 55006 — lock not available / object in use.
    LockContention {
        message: String,
    },

    /// Postgres class 08 (connection_exception) and other transient
    /// failures (53*, 57*). Distinct from `Serialization` because the
    /// retry semantics differ: serialization retries the *transaction*;
    /// transient retries the connection.
    Transient {
        message: String,
    },

    /// Plugin mis-configured at runtime (`DB_URL` missing, advisory
    /// lock unavailable, etc.). Not a user error and not retriable.
    Configuration {
        code: &'static str,
        message: String,
    },

    /// A code already chosen by another subsystem (e.g. the
    /// migrations lifecycle codes in `crate::migrations`) — propagated
    /// verbatim. Lets the typed `DbError` flow through helpers without
    /// flattening to a string at every boundary.
    Coded {
        code: String,
        message: String,
        hint: Option<String>,
    },

    /// Catch-all for everything that doesn't fit above. The message is
    /// preserved for the JS console; the code is always `internal` so
    /// the SDK has *something* to branch on (even if it's "this should
    /// never happen, file a bug").
    Internal {
        message: String,
    },
}

impl DbError {
    /// Classify a `compio_postgres::Error` by SQLSTATE. Falls back to
    /// [`DbError::Internal`] when the error has no code (e.g.
    /// connection-layer errors that aren't class 08). Walks the source
    /// chain so the message reaching JS includes the underlying
    /// `DbError` body, not the bare wrapper kind.
    pub fn from_pg(e: &compio_postgres::Error) -> Self {
        use compio_postgres::error::SqlState;

        let msg = walk_pg_chain(e);

        let Some(code) = e.code() else {
            // No SQLSTATE — usually a connection-layer error
            // (transport, protocol, decode). Treat as transient.
            return DbError::Transient { message: msg };
        };

        if code == &SqlState::UNIQUE_VIOLATION {
            DbError::UniqueViolation { message: msg }
        } else if code == &SqlState::FOREIGN_KEY_VIOLATION {
            DbError::FkViolation { message: msg }
        } else if code == &SqlState::NOT_NULL_VIOLATION {
            DbError::NotNullViolation { message: msg }
        } else if code == &SqlState::CHECK_VIOLATION {
            DbError::CheckViolation { message: msg }
        } else if code == &SqlState::T_R_SERIALIZATION_FAILURE
            || code == &SqlState::T_R_DEADLOCK_DETECTED
        {
            DbError::Serialization { message: msg }
        } else if code == &SqlState::LOCK_NOT_AVAILABLE
            || code == &SqlState::OBJECT_IN_USE
        {
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
            // Unknown SQLSTATE — leave classification to the catch-all
            // but preserve the message so the SDK can debug.
            DbError::Internal { message: msg }
        }
    }

    /// Stamp this `DbError` onto an `OpError` with the canonical
    /// `.code` for the variant. The runtime materialises a JS Error
    /// with `e.code` (and `e.hint` when present) — the SDK reads it
    /// directly.
    pub fn to_op_error(self) -> OpError {
        match self {
            DbError::SchemaRefused { code, envelope_json } => {
                // SchemaRefused carries a JSON envelope the SDK parses
                // verbatim. We stamp the static `code` so SDK callers
                // can branch on `e.code === "validation_refused"` without
                // resorting to JSON.parse(e.message). The envelope JSON
                // stays the message body so existing callers that
                // serde_json::from_str(&err.to_string()) continue to
                // parse the envelope correctly. See `register_model_dispatch`.
                OpError::coded(code, envelope_json, None::<String>)
            }
            DbError::ValidationFailed { code, message, hint } => {
                OpError::coded(code, message, hint)
            }
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
                Some("retry the transaction; Postgres SSI / deadlock detector aborted it".to_string()),
            ),
            DbError::LockContention { message } => OpError::coded(
                "lock_not_available",
                message,
                Some("Retry after a short backoff; another worker holds the lock briefly.".to_string()),
            ),
            DbError::Transient { message } => OpError::coded(
                "transient",
                message,
                Some("transient backend failure; retry after a short backoff".to_string()),
            ),
            DbError::Configuration { code, message } => {
                OpError::coded(code, message, None::<String>)
            }
            DbError::Coded { code, message, hint } => OpError::coded(code, message, hint),
            DbError::Internal { message } => {
                OpError::coded("internal", message, None::<String>)
            }
        }
    }

    /// Render a flat string for callers that still flow through the
    /// `Result<_, String>` -> `OpResult::Failed { error: String }` path
    /// (notably `register_model_dispatch`, where the SDK already
    /// `JSON.parse`'s the message as an envelope). Used as a bridge
    /// while the conversion sweep proceeds; new code should prefer
    /// `to_op_error()`.
    pub fn into_string(self) -> String {
        match self {
            DbError::SchemaRefused { envelope_json, .. } => envelope_json,
            DbError::ValidationFailed { message, .. }
            | DbError::UniqueViolation { message }
            | DbError::FkViolation { message }
            | DbError::NotNullViolation { message }
            | DbError::CheckViolation { message }
            | DbError::Serialization { message }
            | DbError::LockContention { message }
            | DbError::Transient { message }
            | DbError::Configuration { message, .. }
            | DbError::Coded { message, .. }
            | DbError::Internal { message } => message,
        }
    }

    /// Convenience: configuration error with a static `code`.
    pub fn config(code: &'static str, message: impl Into<String>) -> Self {
        DbError::Configuration {
            code,
            message: message.into(),
        }
    }

    /// Convenience: validation error with a static `code`.
    pub fn validation(code: &'static str, message: impl Into<String>) -> Self {
        DbError::ValidationFailed {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// Convenience: validation error with a static `code` + hint.
    pub fn validation_hinted(
        code: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        DbError::ValidationFailed {
            code,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    /// Convenience: catch-all internal error.
    pub fn internal(message: impl Into<String>) -> Self {
        DbError::Internal {
            message: message.into(),
        }
    }
}

/// `Display` renders the same body that `into_string` returns — the
/// message body for the variant (or the JSON envelope for
/// `SchemaRefused`). The `.code` is NOT emitted because Display is
/// used by callers that want the human-readable text (log lines,
/// `format!("{e}")` panics in tests, `serde_json::from_str(&e.to_string())`
/// envelope round-trips); the code surfaces through `to_op_error()`
/// at the V8 boundary, not through Display.
impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::SchemaRefused { envelope_json, .. } => f.write_str(envelope_json),
            DbError::ValidationFailed { message, .. }
            | DbError::UniqueViolation { message }
            | DbError::FkViolation { message }
            | DbError::NotNullViolation { message }
            | DbError::CheckViolation { message }
            | DbError::Serialization { message }
            | DbError::LockContention { message }
            | DbError::Transient { message }
            | DbError::Configuration { message, .. }
            | DbError::Coded { message, .. }
            | DbError::Internal { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for DbError {}

// Postgres-side → DbError so SQL helpers can `?`-flow driver errors
// through the same `to_op_error()` boundary. Routes through
// [`DbError::from_pg`] so SQLSTATE classification is the single source
// of truth — callers gain ergonomic `?` operator while the variant
// selection stays in one place.
impl From<compio_postgres::Error> for DbError {
    fn from(e: compio_postgres::Error) -> Self {
        DbError::from_pg(&e)
    }
}

// Builder-side QueryError → DbError so the dispatch helpers can
// `?`-flow query-construction failures through the same `to_op_error()`
// boundary. Builder errors are user-input refusals (bad filter, bad
// collection name, bad identifier) — modelled as `ValidationFailed`
// with a static code the SDK can branch on.
impl From<crate::query::QueryError> for DbError {
    fn from(e: crate::query::QueryError) -> Self {
        use crate::query::QueryError;
        let (code, msg) = match e {
            QueryError::InvalidFilter(m) => ("invalid_filter", m),
            QueryError::InvalidCollection(m) => ("invalid_collection", m),
            QueryError::InvalidIdent(m) => ("invalid_identifier", m),
        };
        DbError::ValidationFailed {
            code,
            message: msg,
            hint: None,
        }
    }
}

/// Walk the `std::error::Error::source` chain so the JS console sees the
/// underlying Postgres `DbError` body, not the bare wrapper kind. Mirrors
/// the old `fmt_db_err` from `v8_bridge` so the message shape is
/// preserved (`db: <wrapper> — caused by: <cause>`).
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
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint } => {
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
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint } => {
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
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint } => {
                assert_eq!(code, "migration_already_running");
                assert_eq!(hint.as_deref(), Some("y"));
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
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
            (
                DbError::FkViolation { message: "".into() },
                "fk_violation",
            ),
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
            (
                DbError::Transient { message: "".into() },
                "transient",
            ),
            (
                DbError::Internal { message: "".into() },
                "internal",
            ),
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
        assert!(op_hint(DbError::Serialization { message: "x".into() }).is_some());
        assert!(op_hint(DbError::Transient { message: "x".into() }).is_some());
        // LockContention is retriable — must also carry a hint.
        assert!(op_hint(DbError::LockContention { message: "x".into() }).is_some());
        // Non-retryable violations must not advise a retry.
        assert!(op_hint(DbError::UniqueViolation { message: "x".into() }).is_none());
        assert!(op_hint(DbError::FkViolation { message: "x".into() }).is_none());
        assert!(op_hint(DbError::Internal { message: "x".into() }).is_none());
    }

    /// `From<compio_postgres::Error>` must route through
    /// [`DbError::from_pg`] so the SQLSTATE classification stays the
    /// single source of truth. We can't fabricate a real
    /// `compio_postgres::Error` from a `#[test]` without a live
    /// listener (the type's constructors are crate-private), so this
    /// test pins the contract at the type level — if the `From` impl
    /// disappears or its signature drifts, compile fails here.
    #[test]
    fn from_pg_error_impl_is_wired() {
        fn assert_from<T: From<compio_postgres::Error>>() {}
        assert_from::<DbError>();
    }

    /// `From<QueryError>` collapses the builder's three error kinds
    /// onto a `ValidationFailed` with a stable static code the SDK
    /// branches on. Each kind must map to a distinct code.
    #[test]
    fn from_query_error_assigns_distinct_codes() {
        let cases = [
            (
                crate::query::QueryError::InvalidFilter("bad".into()),
                "invalid_filter",
            ),
            (
                crate::query::QueryError::InvalidCollection("bad".into()),
                "invalid_collection",
            ),
            (
                crate::query::QueryError::InvalidIdent("bad".into()),
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
}
