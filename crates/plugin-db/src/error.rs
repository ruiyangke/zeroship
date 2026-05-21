//! Typed error classification for `plugin-db`.
//!
//! Every fallible internal helper that used to return `Result<_, String>`
//! is being migrated to `Result<_, DbError>`. At the V8 boundary the
//! dispatcher calls [`DbError::to_op_error`] to materialise an
//! [`OpError`] whose `.code` is stamped from the variant — the SDK can
//! then branch on `err.code` instead of substring-matching opaque
//! messages.
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
            DbError::SchemaRefused { envelope_json, .. } => {
                // SchemaRefused already carries a JSON envelope the SDK
                // parses verbatim. We stamp the inner `code` field so
                // the runtime path is uniform, but the envelope itself
                // stays the wire payload (callers may need to JSON.parse
                // err.message). See `register_model_dispatch`.
                OpError::error(envelope_json)
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
            DbError::LockContention { message } => {
                OpError::coded("lock_not_available", message, None::<String>)
            }
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
}
