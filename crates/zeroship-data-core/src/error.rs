//! Typed error classification for `plugin-db`.
//!
//! Every fallible internal helper returns `Result<_, DbError>`. At the
//! V8 boundary the dispatcher calls [`DbError::to_op_error`] to
//! materialise an [`OpError`] whose `.code` is stamped from the variant
//! — the SDK can then branch on `err.code` instead of substring-matching
//! opaque messages.
//!
//! `Result<_, String>` is now confined to a small set of deliberate
//! hold-outs across three categories:
//!
//! 1. **Wire-contract envelopes**: the constraint-violation paths, whose
//!    `Err` IS a JSON envelope rather than a message (a documented SDK wire
//!    contract — `JSON.parse(err.message)` recovers the payload). The backends
//!    build it in `backend/postgres.rs` and `backend/sqlite/error.rs` and
//!    return [`DbError::SchemaRefused`]; the static `.code` is stamped from
//!    the variant.
//!
//! 2. **Pure parsers** internal to `auth/session.rs`: `hex_decode` /
//!    `hex_nibble` ASCII-only decoders that never cross an isolate
//!    boundary; lifted into `DbError::internal(...)` at their
//!    call sites. The whole `auth/*` subtree is always compiled but
//!    dormant (no production callers yet); the hold-out class applies
//!    inside the SECURITY DEFINER bootstrap flow once it's wired up.
//!
//! 3. **JS-input arg parsers** in `v8_classes/migration.rs` (`parse_commit_spec`,
//!    `parse_spec`) and `v8_classes/migrations.rs` (`parse_name_and_collection`):
//!    return free-text rejection messages converted to `OpError::type_error`
//!    at the V8 boundary (these are TypeError-class, never need `.code`).
//!
//! 4. **Cold-init**: `lib.rs::init_pool_async` returns `Result<_, String>`;
//!    `exec.rs::ensure_pool` synthesises `DbError::Configuration` with
//!    code `lazy_init_failed` (the same code at
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

/// Classified error origin for every fallible `plugin-db` helper.
///
/// Construct via the variant directly. Vendor tiers translate driver errors
/// into this neutral hierarchy before handing them to core consumers.
#[derive(Debug, Clone)]
// NOT `#[non_exhaustive]`, and that is a decision rather than an omission.
//
// The attribute was here while `DbError` and its `to_op_error` lowering lived
// in ONE crate, where it cost nothing: rustc only demands a wildcard arm across
// a CRATE BOUNDARY. The split put the lowering in `zeroship-plugin-db` and the
// type here, so the attribute would have forced `_ => ...` into
// `op_error.rs` - silently retiring the exhaustiveness check that is the only
// guarantee every variant reaches the V8 boundary with a canonical `.code`
// instead of falling into a default.
//
// `#[non_exhaustive]` buys the freedom to add a variant without breaking
// downstream matches. There is no downstream: zeroship is pre-launch and every
// consumer is in this workspace, so it protects nobody and disables a real
// check. If this crate is ever published, restore it - and expect to hand-audit
// every match on `DbError` from then on.
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
/// On the 5xx allow-list in `crates/runtime/src/core/dispatch.rs` in BOTH
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
pub const MISSING_ROLE_MESSAGE: &str = "this app's database is not provisioned: its per-app Postgres role does not \
     exist. Run `zeroship migrate` for this app, then retry.";

/// Operator/`env.db`-caller hint for [`SCHEMA_NOT_PROVISIONED`]. Reaches app
/// JS as `err.hint` on a direct native throw; does NOT reach the HTTP wire.
pub const MISSING_ROLE_HINT: &str = "`zeroship migrate` creates the app's schema and per-app role. A deploy \
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

    /// Convenience: catch-all internal error.
    pub fn internal(message: impl Into<String>) -> Self {
        DbError::Internal {
            message: message.into(),
        }
    }

    /// Optimistic-concurrency check failed. The UPDATE
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
    pub fn version_mismatch(collection: &str, row_id: Option<&str>, expected_version: i64) -> Self {
        let id_part = row_id.map(|id| format!(" {id}")).unwrap_or_default();
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

    /// UPDATE filter carried `version: N` but no `id`
    /// predicate. The CAS semantics don't generalise cleanly to
    /// multi-row UPDATEs (the affected-rows count conflates "row
    /// missing", "version mismatched", and "filter matched but version
    /// matched" — there's no clean per-row mismatch report). This
    /// refuses the shape eagerly with a typed code so the SDK can
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

    /// Nested `$and` / `$or` `version` predicates are
    /// refused because the CAS path only honours a top-level equality
    /// predicate. Failing closed avoids silently degrading a
    /// compare-and-swap write into a blind last-writer-wins update.
    pub fn version_filter_must_be_top_level(collection: &str) -> Self {
        DbError::ValidationFailed {
            code: "version_filter_must_be_top_level",
            message: format!(
                "UPDATE on `{collection}` requires optimistic-concurrency \
                 `version` filters to be top-level."
            ),
            hint: Some(
                "Use a top-level filter like `{ id: ..., version: N }`; \
                 nested `$and`/`$or` version predicates are refused."
                    .to_string(),
            ),
        }
    }

    /// Build the canonical [`DbError::Configuration`] returned when
    /// `BackendHandle::as_postgres` yields `None` —
    /// i.e. the active backend isn't the Postgres arm. Every call site
    /// (`as_postgres().ok_or_else(...)?` / `Some/None` match) routes
    /// through this helper so the wire `.code` (`backend_unsupported`)
    /// AND the operator-facing `hint` stay identical across backend-specific
    /// operation paths.
    ///
    /// Prior hand-rolled `DbError::Configuration { code:
    /// "backend_unsupported", ... }` literals at four call sites had
    /// drifted into three distinct `hint` shapes; centralising
    /// here keeps the SDK-visible hint stable as the backend-arm
    /// dispatcher evolves.
    ///
    /// `op` names the operation surface for the message body
    /// (e.g. `"transaction"`, `"migration RPC"`)
    /// so the operator-facing text stays specific without forcing each
    /// call site to re-spell the static `code` / `hint`.
    pub fn backend_unsupported(op: &str) -> Self {
        DbError::Configuration {
            code: "backend_unsupported",
            message: format!("`{op}` is not supported by the active database backend"),
            hint: Some(
                "SQLite↔Postgres parity is still being wired; some operations remain backend-specific."
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
/// `auth::session`, `diff`, and `replication`. Operators see "what we
/// were doing when the SQL failed" without losing the SQLSTATE-driven
/// classification at the V8 boundary.
///
/// The set of "prefix-eligible" variants is the SQLSTATE-derived
/// classification set plus `Internal` (the catch-all). The structured
/// variants: `ValidationFailed`, `Configuration`, `PermissionDenied`, `Coded`,
/// and `SchemaRefused` carry their own contracted message bodies (and
/// `.code`s the SDK already branches on) and are intentionally left
/// alone: prefixing them would distort a wire payload the SDK parses
/// verbatim.
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
pub fn first_row_or_internal<'a, R>(
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
impl From<zeroship_schema::query::QueryError> for DbError {
    fn from(e: zeroship_schema::query::QueryError) -> Self {
        use zeroship_schema::query::QueryError;
        // `ReservedSystemFieldName` carries a fixed hint
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
            // UPDATE patch attempted to overwrite an
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

// The mask-sentinel codec
// (`zeroship_schema::mask_codec::parse_mask_sentinel`) was relocated into the
// leaf crate and returns [`zeroship_schema::error::MaskSentinelError`] whose
// `.message` already carries the `mask_sentinel_malformed: …` prefix the SDK
// contract + introspector expect. The pre-extraction parser returned
// `DbError::internal(<that same message>)`; this `From` reproduces it exactly,
// so the `mask_sentinel_malformed` code-discriminator the SDK round-trips is
// preserved.
impl From<zeroship_schema::error::MaskSentinelError> for DbError {
    fn from(e: zeroship_schema::error::MaskSentinelError) -> Self {
        DbError::internal(e.message)
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
