//! Typed error classification for `plugin-db`.
//!
//! Every fallible internal helper returns `Result<_, DbError>`. At the
//! V8 boundary the dispatcher calls [`DbError::to_op_error`] to
//! materialise an [`OpError`] whose `.code` is stamped from the variant
//! — the SDK can then branch on `err.code` instead of substring-matching
//! opaque messages.
//!
//! `Result<_, String>` is now (post-[I28] sweep, commit `0049d9be`)
//! confined to a small set of deliberate hold-outs across three
//! categories:
//!
//! 1. **Wire-contract envelopes**: `crate::orchestrator::register_model::validate`
//!    whose `Err` IS the `validation_refused` JSON envelope (a
//!    documented SDK wire contract — `JSON.parse(err.message)`
//!    recovers the payload). `run_pipeline` wraps it in
//!    [`DbError::SchemaRefused`] at the boundary; the static `.code`
//!    is stamped from the variant.
//!
//! 2. **Pure parsers** internal to `auth/session.rs`: `hex_decode` /
//!    `hex_nibble` ASCII-only decoders that never cross an isolate
//!    boundary; lifted into `DbError::internal(...)` at their
//!    call sites. The whole `auth/*` subtree is cfg-gated behind
//!    `--features hardening` post-`2fa9472e`, so these holdouts are
//!    invisible in default builds — but the hold-out class still
//!    applies inside the SECURITY DEFINER bootstrap flow when the
//!    feature is on.
//!
//! 3. **JS-input arg parsers** in `v8_classes/migration.rs` (`parse_commit_spec`,
//!    `parse_spec`) and `v8_classes/migrations.rs` (`parse_name_and_collection`):
//!    return free-text rejection messages converted to `OpError::type_error`
//!    at the V8 boundary (these are TypeError-class, never need `.code`).
//!
//! 4. **Cold-init**: `lib.rs::init_pool_async` returns `Result<_, String>`;
//!    the call sites at `orchestrator/register_model/mod.rs:120` and
//!    `exec.rs::ensure_pool` synthesise `DbError::Configuration` with
//!    code `lazy_init_failed` (unified at `7d0bc4c5`; same code at
//!    every cold-init call site).
//!
//! 5. **Test helpers** (`exec.rs::exec_query_with_pool_for_tests`,
//!    similar): `#[cfg(any(test, feature = "test-helpers"))]`-gated;
//!    never reach the V8 boundary.
//!
//! Every fallible helper that touches Postgres or the V8 boundary in a
//! production path now returns `Result<_, DbError>` — SDK callers can
//! branch on `err.code` end-to-end on the production code path.
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
    /// lock unavailable, `wal_level != logical`, etc.). Not a user
    /// error and not retriable — the SDK should surface the `.hint`
    /// to the operator since this typically requires a config edit +
    /// restart (error-ux r7 INFO: this is the variant most needing
    /// remediation prose).
    Configuration {
        code: &'static str,
        message: String,
        hint: Option<String>,
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
            DbError::Configuration { code, message, hint } => {
                OpError::coded(code, message, hint)
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

    /// Borrowing variant of [`Self::into_string`] — the message body
    /// without consuming `self`. Used by callers that need to embed the
    /// underlying error text in a wrapping message (e.g. the
    /// `begin_failed` / `commit_failed_indeterminate` wrappers in
    /// [`crate::orchestrator::transaction`]) while keeping the original
    /// `DbError` available.
    pub fn message_str(&self) -> &str {
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

    /// Convenience: configuration error with a static `code` and no
    /// hint. For configs that ship with operator-remediation text,
    /// use [`DbError::config_hinted`].
    pub fn config(code: &'static str, message: impl Into<String>) -> Self {
        DbError::Configuration {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// Configuration error with a remediation hint the SDK can
    /// surface verbatim (e.g. "set wal_level=logical in
    /// postgresql.conf and restart").
    pub fn config_hinted(
        code: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        DbError::Configuration {
            code,
            message: message.into(),
            hint: Some(hint.into()),
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

    /// **P7 PR 4** — optimistic-concurrency check failed. The UPDATE
    /// filter included `version: N` but the row's current `version`
    /// no longer matched (another writer won the race; affected-rows
    /// came back 0).
    ///
    /// SDK callers branch on `e.code === "version_mismatch"`. The
    /// retry advice in the `hint` doubles as the
    /// `OptimisticLockError` message body in the SDK. The proposal's
    /// Q-SF-E (§10) settled on retryable semantics — caller is
    /// expected to re-read the row, observe the bumped `version`,
    /// and retry with the new value.
    pub fn version_mismatch(
        collection: &str,
        row_id: Option<&str>,
        expected_version: i64,
    ) -> Self {
        let id_part = row_id
            .map(|id| format!(" {id}"))
            .unwrap_or_default();
        DbError::ValidationFailed {
            code: "version_mismatch",
            message: format!(
                "Optimistic concurrency check failed for {collection}{id_part}: \
                 expected version {expected_version}, but the row was modified concurrently."
            ),
            hint: Some(
                "Re-read the row to get the current version and retry the update.".to_string(),
            ),
        }
    }

    /// **P7 PR 4** — UPDATE attempted on a table whose schema lacks
    /// the platform-managed `version` column (and by extension the
    /// other six system fields). PR 2 prepends these to every NEW
    /// table; PR 6 will ALTER pre-PR-2 tables in. Until PR 6 lands,
    /// CRUD against a legacy table refuses with this typed code so
    /// the SDK can surface a deterministic message.
    pub fn system_fields_missing(collection: &str) -> Self {
        DbError::ValidationFailed {
            code: "system_fields_missing",
            message: format!(
                "Collection `{collection}` was created before the platform \
                 system-fields contract (`id`, `created_at`, `updated_at`, \
                 `created_by`, `updated_by`, `version`, `deleted_at`) was \
                 introduced; UPDATE auto-bumps `version` and cannot proceed."
            ),
            hint: Some(
                "Re-register the model via `db.registerModel(...)` after \
                 the platform's system-fields migration (PR 6) has run, or \
                 manually ALTER the table to add the seven system field \
                 columns."
                    .to_string(),
            ),
        }
    }

    /// **P7 PR 4** — UPDATE filter carried `version: N` but no `id`
    /// predicate. The CAS semantics don't generalise cleanly to
    /// multi-row UPDATEs (the affected-rows count conflates "row
    /// missing", "version mismatched", and "filter matched but version
    /// matched" — there's no clean per-row mismatch report). PR 4
    /// refuses this shape eagerly with a typed code so the SDK can
    /// guide the creator toward an explicit per-id loop.
    pub fn multi_row_version_filter_unsupported(collection: &str) -> Self {
        DbError::ValidationFailed {
            code: "multi_row_version_filter_unsupported",
            message: format!(
                "UPDATE on `{collection}` with `version` in the filter requires \
                 an `id` predicate; optimistic concurrency is per-row only."
            ),
            hint: Some(
                "Either remove `version` from the filter (last-writer-wins \
                 bulk update) or scope the UPDATE to a single row with \
                 `{ id: ..., version: N }`."
                    .to_string(),
            ),
        }
    }

    /// **P7 PR 5** — `restore()` called on a table that lacks the
    /// `deleted_at` column (no `_systemFields` marker on the cached
    /// schema). The legacy-hard-delete fallback for `delete()` makes
    /// sense (drop a row from a pre-PR-2 table), but there's no
    /// equivalent "soft-restore" target for a table that never had a
    /// `deleted_at` column. Refuse with a typed code so SDK consumers
    /// can surface the remediation message verbatim.
    pub fn restore_unsupported_legacy_table(collection: &str) -> Self {
        DbError::ValidationFailed {
            code: "restore_unsupported_legacy_table",
            message: format!(
                "`{collection}` was created before the platform system-fields \
                 contract (no `deleted_at` column); `restore()` is not \
                 supported on pre-migration tables."
            ),
            hint: Some(
                "Re-register the model via `db.registerModel(...)` after \
                 the platform's system-fields migration (PR 6) has run, or \
                 manually ALTER the table to add the seven system field \
                 columns."
                    .to_string(),
            ),
        }
    }

    /// Build the canonical [`DbError::Configuration`] returned when
    /// [`crate::backend::BackendHandle::as_postgres`] yields `None` —
    /// i.e. the active backend isn't the Postgres arm. Every call site
    /// (`as_postgres().ok_or_else(...)?` / `Some/None` match) routes
    /// through this helper so the wire `.code` (`backend_unsupported`)
    /// AND the operator-facing `hint` stay identical across the
    /// `migrations` / `migration` / `register_model` / `transaction` /
    /// `auto_tx` paths.
    ///
    /// **Post-P0 mop-up (code-critique R15-1 / R15-2)**: prior hand-rolled
    /// `DbError::Configuration { code: "backend_unsupported", ... }`
    /// literals at four call sites had drifted into three distinct
    /// `hint` shapes (two `None`, one PG-only sentence, one bare
    /// `"register_model"`); centralising here keeps the SDK-visible
    /// hint stable as the backend-arm dispatcher evolves.
    ///
    /// `op` names the operation surface for the message body
    /// (e.g. `"transaction"`, `"register_model"`, `"migration RPC"`)
    /// so the operator-facing text stays specific without forcing each
    /// call site to re-spell the static `code` / `hint`.
    pub(crate) fn backend_unsupported(op: &str) -> Self {
        DbError::Configuration {
            code: "backend_unsupported",
            message: format!(
                "`{op}` requires the Postgres backend (SQLite arm lands in P1)"
            ),
            hint: Some(
                "backend is single-arm PG in P0; SQLite arm re-introduced atomically in P1"
                    .to_string(),
            ),
        }
    }
}

/// Prepend a contextual phrase to the human-readable body of `err`
/// while keeping its variant (and therefore its wire `.code`) intact.
///
/// This is the shared primitive every per-module `coded_sql`-style
/// helper routes through: `audit`, `auth::bootstrap`, `auth::keys`,
/// `auth::session`, `diff`, `replication`, and `migrations::coded_db`
/// (deeefe18 — last inline copy collapsed). Operators see "what we
/// were doing when the SQL failed" without losing the SQLSTATE-driven
/// classification at the V8 boundary.
///
/// The set of "prefix-eligible" variants is the SQLSTATE-derived
/// classification set plus `Internal` (the catch-all). The structured
/// variants — `ValidationFailed`, `Configuration`, `Coded`,
/// `SchemaRefused` — carry their own contracted message bodies (and
/// `.code`s the SDK already branches on) and are intentionally left
/// alone: prefixing them would distort a wire payload the SDK parses
/// verbatim.
pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) {
    match err {
        DbError::UniqueViolation { message }
        | DbError::FkViolation { message }
        | DbError::NotNullViolation { message }
        | DbError::CheckViolation { message }
        | DbError::Serialization { message }
        | DbError::LockContention { message }
        | DbError::Transient { message }
        | DbError::Internal { message } => {
            *message = format!("{prefix}{message}");
        }
        // ValidationFailed / Configuration / Coded / SchemaRefused
        // carry their own structured messages and `.code`s the SDK
        // branches on; leaving them alone keeps the wire format
        // verbatim.
        _ => {}
    }
}

/// Classify a `compio_postgres::Error` into a [`DbError`] and prepend a
/// `"<context>: "` phrase to the resulting message body via
/// [`prefix_message`]. The SQLSTATE-derived `.code` is preserved
/// (`unique_violation`, `serialization_failure`, `transient`, …).
///
/// Replaces the per-file `coded_sql` duplicates that lived in `audit`,
/// `auth::bootstrap`, `auth::keys`, `auth::session`, and `diff`. Each
/// caller composes its module-scoped prefix into `context` (e.g.
/// `"audit: INSERT migrations"`, `"diff: probe pg_attribute"`) so the
/// operator-facing message keeps the same shape.
pub(crate) fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    let mut err: DbError = e.into();
    prefix_message(&mut err, &format!("{context}: "));
    err
}

/// Return the first row from a query result slice, or surface a
/// [`DbError::Internal`] naming the operation. Used to close the
/// silent-empty-RETURNING bug class: callers that previously chained
/// `.first().map(...).unwrap_or_default()` coerced an empty RETURNING
/// set into a sentinel value (the audit-id=0 bug fixed in d7cfc089;
/// the replication-slot empty-LSN twin fixed alongside it). The helper
/// names the predicate in one place so every empty-RETURNING site
/// emits the same `DbError::Internal { message: "<op>: returned no
/// row" }` shape — preserving the regression test contract in
/// `audit.rs::tests::insert_backfill_running_empty_returning_is_internal_error`.
///
/// Generic over the row type so test code can exercise the helper
/// without constructing a `compio_postgres::Row` (whose constructors
/// are crate-private). At the production call sites the type is
/// always `&[compio_postgres::Row]`; tests pass `Vec<T>` for any `T`.
pub(crate) fn first_row_or_internal<'a, R>(
    rows: &'a [R],
    op: &'static str,
) -> Result<&'a R, DbError> {
    rows.first().ok_or_else(|| DbError::Internal {
        message: format!("{op}: returned no row"),
    })
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
        // **P7 PR 1** — `ReservedSystemFieldName` carries a fixed hint
        // listing the seven system fields so SDK consumers see the same
        // remediation message Rust prints in test failures. The other
        // three variants carry no hint (builder errors are deterministic
        // identifier shape complaints — the message is self-explanatory).
        let (code, msg, hint) = match e {
            QueryError::InvalidFilter(m) => ("invalid_filter", m, None),
            QueryError::InvalidCollection(m) => ("invalid_collection", m, None),
            QueryError::InvalidIdent(m) => ("invalid_identifier", m, None),
            QueryError::ReservedSystemFieldName(m) => (
                "reserved_system_field_name",
                m,
                Some(
                    "System fields (id, created_at, updated_at, created_by, \
                     updated_by, version, deleted_at) are managed by the \
                     platform and cannot be overridden."
                        .to_string(),
                ),
            ),
            // **P7 PR 4** — UPDATE patch attempted to overwrite an
            // immutable write-once system field (`id`, `created_at`,
            // `created_by`). Distinct code so SDK consumers can branch
            // (e.g. surface a "you can't change the id of a row"
            // remediation) without substring-matching the message.
            QueryError::ImmutableSystemField(m) => (
                "immutable_system_field",
                m,
                Some(
                    "Fields `id`, `created_at`, `created_by` are write-once \
                     and set automatically on INSERT. They cannot be modified \
                     via UPDATE."
                        .to_string(),
                ),
            ),
        };
        DbError::ValidationFailed {
            code,
            message: msg,
            hint,
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

    /// The helper returns the first element of a non-empty slice. The
    /// test uses `Vec<i64>` rather than a real `compio_postgres::Row`
    /// because the driver's `Row` constructors are crate-private; the
    /// helper is generic over `R` precisely so this contract can be
    /// pinned without a live DB.
    #[test]
    fn first_row_or_internal_returns_first_on_non_empty() {
        let rows: Vec<i64> = vec![7, 8, 9];
        let got = first_row_or_internal(&rows, "test op").expect("non-empty");
        assert_eq!(*got, 7);
    }

    /// On an empty slice the helper must produce `DbError::Internal`
    /// whose message names the operation. The audit-id=0 regression
    /// (d7cfc089) is the canonical site this contract protects:
    /// substring matching against the op name is how the in-tree
    /// regression test in `audit.rs` verifies the contract.
    #[test]
    fn first_row_or_internal_returns_internal_err_on_empty() {
        let rows: Vec<i64> = vec![];
        let err = first_row_or_internal(&rows, "audit: INSERT")
            .expect_err("empty slice must error");
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
                DbError::UniqueViolation { message: "boom".into() },
                "unique_violation",
            ),
            (
                DbError::FkViolation { message: "boom".into() },
                "fk_violation",
            ),
            (
                DbError::NotNullViolation { message: "boom".into() },
                "not_null_violation",
            ),
            (
                DbError::CheckViolation { message: "boom".into() },
                "check_violation",
            ),
            (
                DbError::Serialization { message: "boom".into() },
                "serialization_failure",
            ),
            (
                DbError::LockContention { message: "boom".into() },
                "lock_not_available",
            ),
            (
                DbError::Transient { message: "boom".into() },
                "transient",
            ),
            (
                DbError::Internal { message: "boom".into() },
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
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint } => {
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
    /// **P7 PR 1** — the three legacy kinds carry no hint; the new
    /// `ReservedSystemFieldName` variant carries a fixed hint listing
    /// all seven system fields (covered separately by
    /// `from_query_error_reserved_system_field_carries_hint`).
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

    /// **P7 PR 1** — `ReservedSystemFieldName` maps to a distinct
    /// `reserved_system_field_name` code and carries a hint enumerating
    /// the seven system fields.
    #[test]
    fn from_query_error_reserved_system_field_carries_hint() {
        let qe = crate::query::QueryError::ReservedSystemFieldName(
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

    // ---- P7 PR 4 — new typed-error helpers ------------------------

    /// `ImmutableSystemField` maps to the `immutable_system_field`
    /// code with a hint listing the three write-once names.
    #[test]
    fn from_query_error_immutable_system_field_carries_hint() {
        let qe = crate::query::QueryError::ImmutableSystemField(
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
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint } => {
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

    /// `DbError::system_fields_missing` stamps the canonical
    /// `system_fields_missing` code; carries a hint that mentions
    /// PR 6 / registerModel.
    #[test]
    fn system_fields_missing_stamps_canonical_code_and_hint() {
        let e = DbError::system_fields_missing("legacy_posts").to_op_error();
        match &e.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint } => {
                assert_eq!(code, "system_fields_missing");
                let h = hint.as_deref().expect("must carry a remediation hint");
                assert!(
                    h.contains("registerModel") || h.contains("ALTER"),
                    "hint should mention registerModel or ALTER: {h}"
                );
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }

    /// `DbError::multi_row_version_filter_unsupported` stamps the
    /// canonical code; carries a remediation hint pointing at the
    /// per-id loop.
    #[test]
    fn multi_row_version_filter_unsupported_stamps_canonical_code() {
        let e = DbError::multi_row_version_filter_unsupported("posts").to_op_error();
        match &e.kind {
            zeroship_runtime::state::OpErrorKind::CodedError { code, hint } => {
                assert_eq!(code, "multi_row_version_filter_unsupported");
                assert!(hint.is_some(), "must carry a remediation hint");
            }
            other => panic!("expected CodedError, got {other:?}"),
        }
    }
}
