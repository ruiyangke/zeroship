//! Typed errors for ORM operations and connection setup.
//!
//! Database operations and lazy initialization preserve `DbError` through the
//! native API. The V8 adapter's `ToOpError` conversion maps these variants into
//! runtime errors, keeping codes and retry hints available to JavaScript.
//!
//! The wire format JS sees is a JS `Error` with `message` + `code`, plus
//! `hint` when present and `status` when a classification has an HTTP remedy.
//! This layer disciplines the *origin* of those fields so native throws do not
//! reconstruct semantics from message or code strings downstream.
//!
//! ## When to use which variant
//!
//! | Variant | Cause | Example |
//! |---|---|---|
//! | [`DbError::SchemaRefused`] | a write violated a schema constraint | envelope, `.code` names the constraint |
//! | [`DbError::ValidationFailed`] | User-supplied input failed a guardrail | bad isolation level, filter too deep |
//! | [`DbError::UniqueViolation`] | Postgres 23505 | INSERT into UNIQUE index |
//! | [`DbError::FkViolation`] | Postgres 23503 | INSERT references missing parent |
//! | [`DbError::NotNullViolation`] | Postgres 23502 | INSERT/UPDATE without required col |
//! | [`DbError::CheckViolation`] | Postgres 23514 | row failed CHECK constraint |
//! | [`DbError::Serialization`] | Postgres 40001 | SSI conflict in REPEATABLE READ / SERIALIZABLE |
//! | [`DbError::LockContention`] | Postgres 55P03 / lock-not-available | `SELECT … FOR UPDATE NOWAIT` |
//! | [`DbError::Transient`] | Postgres class 08, deadlock, out-of-memory | connection drop, 40P01 |
//! | [`DbError::Configuration`] | Plugin mis-configured, OR the app's own DB was never provisioned | `DB_URL` not set; `schema_not_provisioned` (per-app role missing, fix: `zeroship migrate`) |
//! | [`DbError::PermissionDenied`] | A classified authorization refusal with a terminal HTTP remedy | revoked per-app database grant |
//! | [`DbError::Coded`] | Pre-typed code from another subsystem | migrations.rs `migration_*` codes |
//! | [`DbError::Internal`] | Anything else; logged but stamped `internal` | a `JSON.stringify` that lost a column |

use std::fmt;

/// What the creator asked settlement to do.
///
/// Vocabulary, not logic. The SC-1 reducer DECIDES which intent to settle with;
/// a backend lane only performs it. Both name this type, which is why it is
/// here rather than in either - the same placement `DenyReason` below got when
/// it turned out to be a domain concept the reducer merely used.
///
/// [`Self::verb`] is the ANSI spelling and both supported backends accept it
/// verbatim; a dialect that did not would render its own from the intent rather
/// than have this grow a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleIntent {
    Commit,
    Rollback,
}

impl SettleIntent {
    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Commit => "COMMIT",
            Self::Rollback => "ROLLBACK",
        }
    }
}

/// What the backend actually did with the terminal statement.
///
/// The other half of the settle vocabulary: the intent goes down to the lane,
/// this comes back up. The lane decides WHICH of the three happened, from
/// whatever its dialect told it; the reducer decides what that MEANS for the
/// transaction. Neither type belongs to either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalResult {
    /// The command tag confirmed a commit. The only arm that publishes.
    Committed,
    /// The transaction rolled back - including a `COMMIT` PostgreSQL answered
    /// with the tag `ROLLBACK`, which is a failed transaction, not a
    /// successful one.
    RolledBack,
    /// The terminal statement's outcome cannot be determined. DBR-03: not
    /// knowing is not the same as knowing it ended, so this withdraws the
    /// session rather than assuming a rollback.
    Indeterminate,
}

/// The isolation a creator asked their transaction to run at.
///
/// The four ANSI levels, held as a closed set rather than a validated string.
/// A string would have to be re-validated by anything that trusted it, and
/// would carry one dialect's spelling through code that is supposed to be
/// dialect-free; [`Self::ansi_name`] is the rendering, on the same terms as
/// [`SettleIntent::verb`] - a backend whose spelling differed would render its
/// own rather than have this grow a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl IsolationLevel {
    /// Parse a creator-supplied level, case-insensitively.
    ///
    /// # Errors
    ///
    /// `invalid_isolation_level` when the string names no ANSI level. This is
    /// the ONLY place the creator's string is interpreted; past this point the
    /// value is a variant and cannot be a typo.
    pub fn parse(raw: &str) -> Result<Self, DbError> {
        match raw.to_uppercase().as_str() {
            "READ UNCOMMITTED" => Ok(Self::ReadUncommitted),
            "READ COMMITTED" => Ok(Self::ReadCommitted),
            "REPEATABLE READ" => Ok(Self::RepeatableRead),
            "SERIALIZABLE" => Ok(Self::Serializable),
            _ => Err(DbError::validation(
                "invalid_isolation_level",
                format!(
                    "db.transaction: invalid isolation level: {raw}. Must be one of: \
                     read uncommitted, read committed, repeatable read, serializable"
                ),
            )),
        }
    }

    #[must_use]
    pub const fn ansi_name(self) -> &'static str {
        match self {
            Self::ReadUncommitted => "READ UNCOMMITTED",
            Self::ReadCommitted => "READ COMMITTED",
            Self::RepeatableRead => "REPEATABLE READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }
}

/// How a transaction should be opened.
///
/// **The protocol transports this, not SQL.** SC-1's step configuration used to
/// carry a rendered `BEGIN [ISOLATION LEVEL ...]` string - a PostgreSQL dialect
/// artifact threaded through the vendor-neutral state machine for the benefit of
/// exactly one of the two backends, since the SQLite arm ignored it and sent a
/// hardcoded `BEGIN`. The intent goes down; each lane spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BeginIntent {
    /// Open at the backend's default isolation.
    #[default]
    Default,
    /// Open at a level the creator named.
    Isolation(IsolationLevel),
}

/// Why opening a transaction session failed.
///
/// The fourth member of the backend seam's vocabulary. Keeping the two variants
/// typed is what lets the reducer carry an outcome without parsing the
/// creator-facing error code: a startup failure before or during `BEGIN` is
/// terminal, while one classified at the per-app setup boundary can be
/// retryable - and only the classifier knows which.
#[derive(Debug)]
pub enum OpenSessionError {
    /// The session could not be acquired, or `BEGIN` itself failed.
    Failed(DbError),
    /// The session opened but narrowing its authority did not.
    Setup(SessionSetupError),
}

impl From<DbError> for OpenSessionError {
    fn from(error: DbError) -> Self {
        Self::Failed(error)
    }
}

impl From<SessionSetupError> for OpenSessionError {
    fn from(error: SessionSetupError) -> Self {
        Self::Setup(error)
    }
}

/// What a forced cleanup proved about the session it acted on.
///
/// The third member of the backend seam's vocabulary, beside [`SettleIntent`]
/// and [`TerminalResult`]. A cleanup is the one operation whose ANSWER is
/// weaker than its request: the reducer asks for a rollback and gets back what
/// the backend could actually establish, which on PostgreSQL is a command
/// result plus a status byte and on SQLite is `is_autocommit` sampled inside the
/// actor. The lane decides which of the three it can prove; the reducer decides
/// whether that discharges the cleanup goal.
///
/// **The distinction between the first two arms is load-bearing and not
/// cosmetic.** `NoOpenTransaction` is the weaker proof - the session is
/// provably outside any block, but this cleanup is not what put it there - and
/// SC-1 accepts it for some goals and not others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupAck {
    /// The session reports no open transaction.
    NoOpenTransaction,
    /// The session reports the transaction was rolled back.
    RolledBack,
    /// The oracle cannot say.
    Indeterminate,
}

/// Why a `Deny` was returned. Every reason is creator-visible, distinct, and
/// non-retryable, and they differ in what the next action should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// A permanent tombstone. There is nothing to re-resolve to.
    AppDeprovisioned,
    /// The subject is alive under a new incarnation, or the record answering
    /// names a different subject entirely. This handle is dead; a freshly
    /// resolved binding is not, so re-resolving is the correct next action.
    ///
    /// A caller that cannot tell this from a tombstone either re-resolves
    /// against a deprovisioned app forever or gives up on a live one.
    StaleAppIncarnation,
    /// The cluster or timeline answering is not the one the binding captured.
    /// Re-resolving locally does not help; this is an operational fault, not a
    /// lifecycle event.
    AuthorityDomainMismatch,
    /// PostgreSQL refused the session's `SET LOCAL ROLE` because the worker
    /// login no longer holds the app-role membership. Authorization already
    /// failed closed; this reason supplies the terminal remedy.
    GrantRevoked,
}

impl DenyReason {
    /// The creator-visible code. These reach the caller as typed terminal
    /// errors, never as an audit row only.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::AppDeprovisioned => "APP_DEPROVISIONED",
            Self::StaleAppIncarnation => "STALE_APP_INCARNATION",
            Self::AuthorityDomainMismatch => "AUTHORITY_DOMAIN_MISMATCH",
            Self::GrantRevoked => "GRANT_REVOKED",
        }
    }

    /// None of the reasons is retryable. That is the point of a terminal denial:
    /// none of them is improved by trying again.
    ///
    /// Non-retryable is **not** the same as indistinguishable - see the
    /// variants' own docs for what each tells the caller to do instead.
    #[must_use]
    pub const fn retryable(self) -> bool {
        false
    }

    /// The reasons the authority-observation classifier itself can return.
    pub const AUTHORITY: [Self; 3] = [
        Self::AppDeprovisioned,
        Self::StaleAppIncarnation,
        Self::AuthorityDomainMismatch,
    ];

    /// Every reason from either an authority observation or classified session
    /// setup, for tests that rule on the closed set.
    pub const ALL: [Self; 4] = [
        Self::AppDeprovisioned,
        Self::StaleAppIncarnation,
        Self::AuthorityDomainMismatch,
        Self::GrantRevoked,
    ];
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Backend-neutral database errors shared by ORM callers and host adapters.
#[derive(Debug, Clone)]
// Keep this enum exhaustive so host adapters must map every variant explicitly.
pub enum DbError {
    /// A backend constraint refusal represented as a JSON envelope. The full
    /// envelope is in `envelope_json`; the variant exists so callers know the wire
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
    UniqueViolation { message: String },

    /// Postgres 23503 — foreign key constraint violation.
    FkViolation { message: String },

    /// Postgres 23502 — NOT NULL violation.
    NotNullViolation { message: String },

    /// Postgres 23514 — CHECK constraint violation.
    CheckViolation { message: String },

    /// Postgres 40001 / 40P01 — serialization failure or deadlock.
    /// SDK callers can retry these via the standard exponential
    /// backoff loop.
    Serialization { message: String },

    /// Postgres 55P03 / 55006 — lock not available / object in use.
    LockContention { message: String },

    /// Postgres class 08 (connection_exception) and other transient
    /// failures (53*, 57*). Distinct from `Serialization` because the
    /// retry semantics differ: serialization retries the *transaction*;
    /// transient retries the connection.
    Transient { message: String },

    /// Plugin mis-configured at runtime (`DB_URL` missing, advisory
    /// lock unavailable, `wal_level != logical`, etc.). Not a user
    /// error and not retriable — the SDK should surface the `.hint`
    /// to the operator since this typically requires a config edit +
    /// restart (this is the variant most needing
    /// remediation prose).
    Configuration {
        code: &'static str,
        message: String,
        hint: Option<String>,
    },

    /// A classified authorization refusal. Unlike a generic coded error, this
    /// carries the semantic 403 remedy through the native V8 boundary.
    PermissionDenied {
        code: &'static str,
        message: &'static str,
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

    /// A platform-internal capability was reached through a
    /// creator-visible path that must refuse it. Today the sole source is
    /// the `env.db.__platform` string-access getter trap
    /// (`platform_internal_only`): the real `DbPlatform` capability handle
    /// lives under a V8 private symbol and is unreachable from creator JS,
    /// so any string-named `__platform` access is an attempt to reach a
    /// platform internal and is denied with a `tracing::error!`. The
    /// `code` is `'static` because the closed set of access-denied
    /// reasons is known at compile time.
    AccessDenied { code: &'static str },

    /// Catch-all for everything that doesn't fit above. The message is
    /// preserved for the JS console; the code is always `internal` so
    /// the SDK has *something* to branch on (even if it's "this should
    /// never happen, file a bug").
    Internal { message: String },
}

/// How a per-app session-setup failure must drive transaction startup.
///
/// The disposition is captured at the `SET LOCAL ROLE` provenance boundary,
/// while the SQLSTATE is still available. Downstream code must never recover
/// it by matching a creator-visible error code string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSetupDisposition {
    /// Preserve the classified [`DbError`] as-is while ending this attempt.
    Preserve,
    /// End this attempt and re-resolve its authority. No producer selects this
    /// yet; an epoch fence can use it without changing the begin event again.
    #[allow(dead_code, reason = "reserved for the schema-epoch producer task")]
    ReResolve,
    /// End this attempt with a specific terminal denial.
    Denied(DenyReason),
    /// No setup-specific classification applied.
    Failed,
}

/// A session-setup error plus the reducer outcome chosen at its provenance
/// boundary.
#[derive(Debug, Clone)]
pub struct SessionSetupError {
    disposition: SessionSetupDisposition,
    error: DbError,
}

impl SessionSetupError {
    pub fn new(disposition: SessionSetupDisposition, error: DbError) -> Self {
        Self { disposition, error }
    }

    pub fn failed(error: DbError) -> Self {
        Self::new(SessionSetupDisposition::Failed, error)
    }

    /// The transaction-startup outcome. This is semantic state, not a wire
    /// error code to be decoded later.
    #[must_use]
    pub const fn disposition(&self) -> SessionSetupDisposition {
        self.disposition
    }

    pub fn error_mut(&mut self) -> &mut DbError {
        &mut self.error
    }

    /// Discard the transaction disposition for a caller, such as autocommit,
    /// that only needs the creator-facing database error.
    #[must_use]
    pub fn into_db_error(self) -> DbError {
        self.error
    }
}

/// Public error code for "this app's database was never provisioned".
///
/// On the 5xx allow-list in `crates/zeroship-runtime/src/core/dispatch.rs` in BOTH
/// spellings: `@zeroship/db` re-stamps every native code through
/// `canonicalErrorCode` inside the isolate, so a creator using the SDK sees
/// `SCHEMA_NOT_PROVISIONED` and a creator calling `env.db` directly sees this
/// one. Both must be listed or the exemption is inert on the path creators
/// actually take.
pub const SCHEMA_NOT_PROVISIONED: &str = "schema_not_provisioned";

/// Public error code for a session whose database-role membership was revoked.
pub const GRANT_REVOKED: &str = DenyReason::GrantRevoked.code();

/// Fixed creator-facing message for [`GRANT_REVOKED`]. The PostgreSQL message
/// and role name remain in the operator log.
pub const GRANT_REVOKED_MESSAGE: &str =
    "this app's database grant has been revoked. Restore the database grant before retrying.";

/// The wire message for [`SCHEMA_NOT_PROVISIONED`]. Platform-authored and
/// fixed: it names the condition and the exact command that fixes it, and it
/// interpolates NOTHING. The server text and the role name (which embeds the
/// app id) stay in the operator log.
///
/// The remediation lives in the MESSAGE, not the hint, because `hint` is
/// populated by `OpError::coded` and then dropped -- `build_verbose_error_body`
/// emits `message`/`name`/`code`/`details`/`retryable` and never `hint`. A
/// creator reading the HTTP response only ever sees the message.
pub const MISSING_ROLE_MESSAGE: &str =
    "this app's database is not provisioned: its per-app Postgres role does not \
     exist. Run `zeroship migrate` for this app, then retry.";

/// Operator/`env.db`-caller hint for [`SCHEMA_NOT_PROVISIONED`]. Reaches app
/// JS as `err.hint` on a direct native throw; does NOT reach the HTTP wire.
pub const MISSING_ROLE_HINT: &str =
    "`zeroship migrate` creates the app's schema and per-app role. A deploy \
     alone does not: the first `env.db` call is what discovers the role is \
     missing.";

impl DbError {
    /// Render a flat string for remaining non-V8 and test callers. New V8
    /// boundary code should prefer `to_op_error()` so the typed code survives.
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
            DbError::PermissionDenied { message, .. } => message.to_string(),
            DbError::AccessDenied { .. } => {
                "platform-internal capability is not reachable from app code".to_string()
            }
        }
    }

    /// Borrowing variant of [`Self::into_string`] — the message body
    /// without consuming `self`. Used by callers that need to embed the
    /// underlying error text in a wrapping message (e.g. the
    /// `begin_failed` / `commit_failed_indeterminate` wrappers in
    /// `transaction`) while keeping the original
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
            DbError::PermissionDenied { message, .. } => message,
            DbError::AccessDenied { .. } => {
                "platform-internal capability is not reachable from app code"
            }
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

    /// A database value cannot be represented by the native row contract.
    /// Name the column and cause, without copying stored values into errors.
    pub fn row_decode(column: &str, reason: &str) -> Self {
        Self::Coded {
            code: "row_decode_failed".into(),
            message: format!("db: cannot decode column '{column}': {reason}"),
            hint: None,
        }
    }

    /// Convenience: catch-all internal error.
    pub fn internal(message: impl Into<String>) -> Self {
        DbError::Internal {
            message: message.into(),
        }
    }

    /// A descriptor-declared optimistic-concurrency check failed.
    pub fn concurrency_mismatch(
        collection: &str,
        row_id: Option<&str>,
        column: &str,
        expected: i64,
    ) -> Self {
        let id_part = row_id.map(|id| format!(" {id}")).unwrap_or_default();
        DbError::ValidationFailed {
            code: "concurrency_mismatch",
            message: format!(
                "Optimistic concurrency check failed for {collection}{id_part}: \
                 expected `{column}` value {expected}, but the row was modified concurrently."
            ),
            hint: Some(format!(
                "Re-read the row to get the current `{column}` value and retry the update."
            )),
        }
    }

    /// A concurrency guard without an identity cannot report per-row conflicts.
    pub fn multi_row_concurrency_filter_unsupported(collection: &str, column: &str) -> Self {
        DbError::ValidationFailed {
            code: "multi_row_concurrency_filter_unsupported",
            message: format!(
                "UPDATE on `{collection}` with `{column}` in the filter requires \
                 an `id` predicate; optimistic concurrency is per-row only."
            ),
            hint: Some(format!(
                "Either remove `{column}` from the filter (last-writer-wins \
                 bulk update) or scope the UPDATE to a single row with \
                 `{{ id: ..., {column}: expected }}`."
            )),
        }
    }

    /// Concurrency guards must be direct equality predicates.
    pub fn concurrency_filter_must_be_top_level(collection: &str, column: &str) -> Self {
        DbError::ValidationFailed {
            code: "concurrency_filter_must_be_top_level",
            message: format!(
                "UPDATE on `{collection}` requires optimistic-concurrency \
                 `{column}` filters to be top-level."
            ),
            hint: Some(format!(
                "Use a top-level filter like `{{ id: ..., {column}: expected }}`; \
                 nested `$and`/`$or` guards are refused."
            )),
        }
    }
}

/// Add context to database failure messages while preserving their classification.
/// Structured validation, configuration and policy errors retain their original bodies.
pub fn prefix_message(err: &mut DbError, prefix: &str) {
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
        // ValidationFailed / Configuration / PermissionDenied / Coded / SchemaRefused
        // carry their own structured messages and `.code`s the SDK
        // branches on; leaving them alone keeps the wire format
        // verbatim.
        _ => {}
    }
}

/// Return the first row from a query result slice, or surface a
/// [`DbError::Internal`] naming the operation. Used to close the
/// silent-empty-RETURNING bug class: callers that previously chained
/// `.first().map(...).unwrap_or_default()` coerced an empty RETURNING
/// set into a sentinel value (the audit-id=0 bug; the replication-slot
/// empty-LSN twin was fixed alongside it). The helper
/// names the predicate in one place so every empty-RETURNING site
/// emits the same `DbError::Internal { message: "<op>: returned no
/// row" }` shape for every empty-`RETURNING` site.
///
/// Generic over the row type so core does not need to know any backend's row
/// representation and test code can exercise the helper with plain values.
pub fn first_row_or_internal<'a, R>(rows: &'a [R], op: &'static str) -> Result<&'a R, DbError> {
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
            DbError::PermissionDenied { message, .. } => f.write_str(message),
            DbError::AccessDenied { .. } => {
                f.write_str("platform-internal capability is not reachable from app code")
            }
        }
    }
}

impl std::error::Error for DbError {}

impl From<zeroship_core::database_role::PerAppRoleNameError> for DbError {
    fn from(error: zeroship_core::database_role::PerAppRoleNameError) -> Self {
        Self::internal(format!("db: {error}"))
    }
}

// Builder-side QueryError → DbError so the dispatch helpers can
// `?`-flow query-construction failures through the same `to_op_error()`
// boundary. Builder errors are user-input refusals (bad filter, bad
// collection name, bad identifier) — modelled as `ValidationFailed`
// with a static code the SDK can branch on.
impl From<crate::sql::mapping::QueryError> for DbError {
    fn from(e: crate::sql::mapping::QueryError) -> Self {
        use crate::sql::mapping::QueryError;
        let (code, msg, hint) = match e {
            QueryError::InvalidFilter(m) => ("invalid_filter", m, None),
            QueryError::InvalidCollection(m) => ("invalid_collection", m, None),
            QueryError::InvalidIdent(m) => ("invalid_identifier", m, None),
            QueryError::ReservedIdPrefix(m) => (
                "reserved_id_prefix", m,
                Some("Choose an ID prefix outside the platform-reserved namespace.".into()),
            ),
            QueryError::ImmutableAssignedField(m) => (
                "immutable_assigned_field", m,
                Some("This field is assigned by its descriptor and cannot be changed through UPDATE.".into()),
            ),
        };
        DbError::ValidationFailed {
            code,
            message: msg,
            hint,
        }
    }
}

// The mask-sentinel codec
// (`crate::sql::mask_codec::parse_mask_sentinel`) was relocated into the
// leaf crate and returns [`crate::sql::schema_error::MaskSentinelError`] whose
// `.message` already carries the `mask_sentinel_malformed: …` prefix the SDK
// contract + introspector expect. The pre-extraction parser returned
// `DbError::internal(<that same message>)`; this `From` reproduces it exactly,
// so the `mask_sentinel_malformed` code-discriminator the SDK round-trips is
// preserved.
impl From<crate::sql::schema_error::MaskSentinelError> for DbError {
    fn from(e: crate::sql::schema_error::MaskSentinelError) -> Self {
        DbError::internal(e.message)
    }
}

impl From<crate::sql::codecs::CodecError> for DbError {
    fn from(error: crate::sql::codecs::CodecError) -> Self {
        match error {
            crate::sql::codecs::CodecError::Internal { message } => Self::internal(message),
            crate::sql::codecs::CodecError::Validation { code, message } => {
                Self::validation(code, message)
            }
            crate::sql::codecs::CodecError::Decode { column, reason } => {
                Self::row_decode(&column, reason)
            }
        }
    }
}

impl From<zeroship_core::schema_name::SchemaNameError> for DbError {
    fn from(error: zeroship_core::schema_name::SchemaNameError) -> Self {
        crate::sql::mapping::QueryError::InvalidCollection(error.to_string()).into()
    }
}

#[cfg(test)]
mod isolation_level_tests {
    use super::{DbError, IsolationLevel};

    /// The validation half of what `transaction::build_begin_sql` used to do
    /// in the engine, before the protocol stopped transporting SQL. Parsing is
    /// the ONLY place a creator's string is interpreted; the rendering half is
    /// `backend::postgres::render_begin`.
    #[test]
    fn every_ansi_level_parses_case_insensitively() {
        assert_eq!(
            IsolationLevel::parse("SERIALIZABLE").unwrap(),
            IsolationLevel::Serializable
        );
        assert_eq!(
            IsolationLevel::parse("read committed").unwrap(),
            IsolationLevel::ReadCommitted
        );
        assert_eq!(
            IsolationLevel::parse("Repeatable Read").unwrap(),
            IsolationLevel::RepeatableRead
        );
        assert_eq!(
            IsolationLevel::parse("READ UNCOMMITTED").unwrap(),
            IsolationLevel::ReadUncommitted
        );
    }

    #[test]
    fn an_unknown_level_is_a_validation_refusal() {
        let err = IsolationLevel::parse("bananas").unwrap_err();
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_isolation_level");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// A round trip, which is what makes the two halves one rule rather than
    /// two lists that can drift apart.
    #[test]
    fn ansi_name_round_trips_through_parse() {
        for level in [
            IsolationLevel::ReadUncommitted,
            IsolationLevel::ReadCommitted,
            IsolationLevel::RepeatableRead,
            IsolationLevel::Serializable,
        ] {
            assert_eq!(IsolationLevel::parse(level.ansi_name()).unwrap(), level);
        }
    }
}
