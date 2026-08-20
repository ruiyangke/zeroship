//! The single source of truth for every DTO that crosses the N-API boundary.
//!
//! Two families live here, both `#[napi(object)]` so napi-rs emits ONE `index.d.ts`
//! the TS host imports (no hand-copied interfaces):
//!
//! 1. **Driver cell transport** ([`JsCell`]/[`JsRow`]/[`JsReply`]/[`JsError`]/
//!    [`JsRequest`]) — the neutral `{ kind, sql, binds, textParams }` verb the engine
//!    hands the host `pg`/`mysql2` driver and the `{ rows, rowCount }` reply it gets
//!    back. Plain owned data (`String`/`i64`/`Vec`/`bool`), all
//!    `Send + 'static`, so they ride a `ThreadsafeFunction` call + a `done` callback
//!    without ever carrying an engine `!Send` handle. These compile WITHOUT
//!    the `napi` feature (the `#[cfg_attr]` gate) so the napi-free mock-apply
//!    integration test constructs them directly.
//!
//! 2. **Typed verb request/response envelopes** ([`ApplyRequest`]/[`StatusRequest`]/
//!    [`StatusIrRequest`]/[`HistoryRequest`]/[`ApplyReply`]/[`StatusReply`]/[`HistoryReply`]/
//!    [`LoadVerifyReply`]) — the strongly-typed shape each `#[napi]` verb TAKES and
//!    RETURNS. The IR `ops` AST and a lowered `Migration` cross as REAL JS
//!    values (`serde-json` feature, [`JsonValue`]); the exact-integer audit fields
//!    (`event_seq`) cross as JS `bigint` (napi6 [`BigInt`]). The request envelopes
//!    are napi-gated (they carry napi-only field types); the reply envelopes carry
//!    the projected engine result.
//!
//! ## The exact-integer story (napi6)
//! - DB *cell values* keep the [`JsCell`] `int`/`intStr` split — that is the
//!   contract for column values (a poisoned global `pg` type-parser must not truncate
//!   an `int8` below the seam). It is orthogonal to the count/sequence question.
//! - A driver's affected-/returned-row *count* is an honest [`i64`]: node-pg/mysql2
//!   report `rowCount` as a JS `number`; `i64` reads it losslessly with no float games.
//! - The journal's monotonic `event_seq` is an [`i64`] read from an `int8` column; it
//!   crosses to JS as a `bigint` ([`BigInt`]) in [`HistoryEventDto`] so a large
//!   sequence never rounds.
//!
//! [`JsCell`]: crate::wire::JsCell
//! [`JsRow`]: crate::wire::JsRow
//! [`JsReply`]: crate::wire::JsReply
//! [`JsError`]: crate::wire::JsError
//! [`JsRequest`]: crate::wire::JsRequest
//! [`ApplyRequest`]: crate::wire::ApplyRequest
//! [`StatusRequest`]: crate::wire::StatusRequest
//! [`StatusIrRequest`]: crate::wire::StatusIrRequest
//! [`HistoryRequest`]: crate::wire::HistoryRequest
//! [`ApplyReply`]: crate::wire::ApplyReply
//! [`StatusReply`]: crate::wire::StatusReply
//! [`HistoryReply`]: crate::wire::HistoryReply
//! [`LoadVerifyReply`]: crate::wire::LoadVerifyReply
//! [`JsonValue`]: crate::wire::JsonValue
//! [`BigInt`]: napi::bindgen_prelude::BigInt
//! [`HistoryEventDto`]: crate::wire::HistoryEventDto

#[cfg(feature = "napi")]
use napi::bindgen_prelude::BigInt;
#[cfg(feature = "napi")]
use napi_derive::napi;

/// A `serde_json::Value` crossing the boundary as a REAL JS value (object/array/…),
/// enabled by napi's `serde-json` feature — the IR `ops` AST and a lowered
/// `Migration` ride this instead of a re-serialized JSON string.
///
/// The ALIAS is napi-neutral even though the crossing it names is not: it is
/// `serde_json::Value` either way, and the napi-neutral descriptor DTOs below use it
/// for their sub-object facets, so gating it would gate them back to napi-only.
pub type JsonValue = serde_json::Value;

/// Restore exact integers that napi's JS-number conversion widened to `f64`.
///
/// A JS number crossing as a real value is `f64` once it no longer fits a `u32`, so
/// `4294967296` arrives as `4294967296.0` and re-serializes with the decimal point.
/// The IR deserializer then takes its "fractional or exponential" branch and
/// refuses a value that is nowhere near the `|v| < 2^53` limit it actually enforces.
///
/// The effect was a lint/apply split, because the two host boundaries differ:
/// `validate` hands the addon `JSON.stringify(envelope)`, where the integer
/// survives as text, while `apply` crosses the envelope as a JS value. A migration
/// inserting a millisecond timestamp (~1.78e12, far above `u32::MAX`) therefore
/// linted clean and died at deploy.
///
/// Whole integers below 2^53 are converted back; anything at or beyond it is left
/// alone so the engine's own refusal — which names the `{"int64":"…"}` carrier —
/// still fires. `f as i64` is exact in that range, and a value like
/// `9007199254740993` is not representable as `f64` at all, so it never reaches the
/// conversion.
#[cfg(feature = "napi")]
pub(crate) fn restore_exact_integers(value: &mut JsonValue) {
    /// `2^53`. Matches `zero_migrate_ir`'s `MAX_EXACT_INT`, the point past which an
    /// integer must use the exact carrier rather than a JS number.
    const MAX_EXACT: f64 = 9_007_199_254_740_992.0;
    match value {
        JsonValue::Number(number) => {
            // Already an exact integer to serde_json: nothing to restore.
            if number.as_i64().is_some() || number.as_u64().is_some() {
                return;
            }
            if let Some(float) = number.as_f64() {
                if float.fract() == 0.0 && float.abs() < MAX_EXACT {
                    *number = serde_json::Number::from(float as i64);
                }
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                restore_exact_integers(item);
            }
        }
        JsonValue::Object(entries) => {
            for (_, entry) in entries.iter_mut() {
                restore_exact_integers(entry);
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::String(_) => {}
    }
}

// ===========================================================================
// 1. Driver cell transport DTOs (napi-neutral — the mock test builds them).
// ===========================================================================

/// The kind tag of a driver-neutral scalar cell — mirrors the engine `Value`'s
/// variants.
///
/// A `#[napi(object)]` cannot be a Rust enum with data, so a cell crosses as a
/// tagged struct: `kind` selects the arm, and the matching payload field carries
/// the value (all others `None`/default). This keeps the union explicit on both
/// sides without a fragile positional encoding.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsCell {
    /// `"null" | "text" | "int" | "bool" | "textArray"`.
    pub kind: String,
    /// Payload for `kind == "text"`.
    pub text: Option<String>,
    /// Payload for `kind == "int"` when the value fits a JS `number` safely.
    pub int: Option<f64>,
    /// Payload for `kind == "int"` carried as a decimal string (int8/numeric that a
    /// `pg` type-parser stringified). Preferred over `int` when present.
    pub int_str: Option<String>,
    /// Payload for `kind == "bool"`.
    pub bool: Option<bool>,
    /// Payload for `kind == "textArray"`; a JS `null` element is `None`.
    pub text_array: Option<Vec<Option<String>>>,
}

/// A driver-neutral row crossing the boundary: parallel column-name / cell vectors.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsRow {
    /// Column names, positionally aligned with `cells`.
    pub columns: Vec<String>,
    /// One [`JsCell`] per column.
    pub cells: Vec<JsCell>,
}

/// A successful verb reply crossing the boundary: the returned rows (empty for a
/// pure DML `execute`/`executeTextParams`) plus the driver's affected-/returned-row
/// count (`result.rowCount` from node-pg). The engine's `execute` verbs read the
/// count; `query`/`queryOne` read the rows.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsReply {
    /// Rows for `query`/`queryOne` (empty for pure DML).
    pub rows: Vec<JsRow>,
    /// `result.rowCount` — affected/returned rows. An honest [`i64`] (a JS `number`);
    /// `None` when the driver reports no count (e.g. `batch`). A count is an integer,
    /// not a float.
    pub row_count: Option<i64>,
}

/// A driver-neutral error crossing the boundary: message + optional SQLSTATE.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsError {
    /// Human-readable message (`err.message` from node-pg).
    pub message: String,
    /// SQLSTATE if the driver surfaced one (`err.code` from node-pg).
    pub code: Option<String>,
}

/// A single verb request the engine hands to the host driver.
///
/// `kind` selects the verb (`batch | execute | executeTextParams | query |
/// queryOne`); `sql` is the statement; `binds` carries the neutral params for
/// `execute`/`query`/`queryOne`; `textParams` carries the `&[Option<String>]`
/// text-format params for `executeTextParams` (text-format, server-inferred
/// OID). Exactly one of `binds`/`textParams` is populated per verb kind.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct JsRequest {
    /// The verb: `"batch" | "execute" | "executeTextParams" | "query" | "queryOne"`.
    pub kind: String,
    /// The SQL statement.
    pub sql: String,
    /// Neutral binds for `execute`/`query`/`queryOne` (as [`JsCell`]s).
    pub binds: Vec<JsCell>,
    /// Text-format params for `executeTextParams` (`None` element → SQL NULL bind).
    pub text_params: Vec<Option<String>>,
}

// ===========================================================================
// 2. Typed verb request/response envelopes. A DTO is napi-gated only when it
//    carries a napi-only field type (`JsonValue`, `BigInt`): those are the
//    request envelopes and `HistoryEventDto`. The reply DTOs below are plain
//    owned data, so they build without the feature and `crate::verbs` projects
//    into them under a napi-free test.
// ===========================================================================

/// The typed request for the host-authoring `applyIr` verb.
///
/// The `envelope` (`{ ir_version, name, ops }`) crosses as a REAL JS value
/// ([`JsonValue`]) — the recorder builds a JS object, no JSON string round-trip. The
/// envelope MUST NOT carry `owner_app` (it is stamped from `owner_app` here —
/// provenance).
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ApplyRequest {
    /// The deploying app id (`app_…`) — stamped as `owner_app` + folded into the
    /// authoritative `Checksum::of_ir` in Rust.
    pub owner_app: String,
    /// The confined project schema the lower pins ops to.
    pub project_schema: String,
    /// The migrator role to `SET ROLE` under (least-privilege apply). Optional.
    pub migrator_role: Option<String>,
    /// `"postgres" | "mysql"` — selects the dialect backend (`SQLite` is in-process).
    pub dialect: String,
    /// The project's `{ table: owner_app }` ownership registry. Empty on a
    /// fresh single-app project.
    pub registry: std::collections::HashMap<String, String>,
    /// The pure-JS IR envelope `{ ir_version, name, ops }` as a JS value.
    pub envelope: JsonValue,
    /// Ordered authored envelopes that precede `envelope` in the project migration
    /// set. Apply uses them only to reconstruct declared logical column contracts,
    /// and accepts that metadata only after the corresponding plans are proven
    /// fully applied in the journal.
    pub prior_envelopes: Option<Vec<JsonValue>>,
    /// The **policy input**: an ordered list of policy charter documents (TOML).
    /// The first document is the root bound; each subsequent document narrows it.
    pub charter_layers: Vec<String>,
    /// Whether destructive changes are pre-approved.
    pub approved: bool,
    /// The audit `applied_by` label recorded in the journal.
    pub applied_by: String,
}

/// The typed request for the in-process SQLite `applyIrSqlite` verb.
///
/// Unlike [`ApplyRequest`], this carries the complete ordered envelope sequence:
/// SQLite opens its bundled-rusqlite backend in the addon and deploys every
/// pending envelope in one engine call, without a host-driver callback.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ApplyIrSqliteRequest {
    /// The deploying app id (`app_…`) stamped onto every lowered migration.
    pub owner_app: String,
    /// The logical project/schema name used by lowering and executor confinement.
    pub project_schema: String,
    /// The project's `{ table: owner_app }` ownership registry.
    pub registry: std::collections::HashMap<String, String>,
    /// Ordered policy charter documents (TOML), starting with the root bound.
    pub charter_layers: Vec<String>,
    /// Whether destructive changes are pre-approved.
    pub approved: bool,
    /// Ordered authored migration IR envelopes as real JavaScript values.
    pub envelopes: Vec<JsonValue>,
}

/// The typed request for the `status` verb.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct StatusRequest {
    /// The project id the `ExecutorConfig` carries.
    pub project_id: String,
    /// The confined project schema.
    pub project_schema: String,
    /// `"postgres" | "mysql"` selects the matching journal backend.
    pub dialect: String,
    /// The pre-lowered migrations to reconcile against the journal, each a
    /// `Migration` as a JS value (empty array for the read/empty-journal flow).
    pub migrations: Vec<JsonValue>,
    /// Ordered policy charter documents (TOML), starting with the root bound and
    /// used by the executor's guard.
    pub charter_layers: Vec<String>,
}

/// The typed request for the plan-aware `statusIr` verb.
///
/// Each envelope is lowered through the same guarded Rust path as `applyIr`; the
/// resulting full plan retains DML and backfill identities instead of projecting
/// them to a lossy `Vec<Migration>`.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct StatusIrRequest {
    /// The deploying app id stamped during guarded lowering.
    pub owner_app: String,
    /// The confined project schema.
    pub project_schema: String,
    /// `"postgres" | "mysql" | "sqlite"` selects the journal backend and must
    /// match the host-driven or in-process status entrypoint.
    pub dialect: String,
    /// The project's table-ownership registry.
    pub registry: std::collections::HashMap<String, String>,
    /// Ordered authored migration envelopes to reconcile.
    pub envelopes: Vec<JsonValue>,
    /// Required ordered policy charters, identical to the `applyIr` lowering input.
    pub charter_layers: Vec<String>,
    /// Reconcile without bootstrapping journal objects. Defaults to `false` when
    /// omitted so existing status callers retain their creating behavior.
    pub read_only: Option<bool>,
}

/// The typed request for completing or aborting one outstanding PostgreSQL
/// online-rename contract.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ResolvePendingRequest {
    /// The app id that authored the original rename. It is required to reproduce
    /// the engine-stamped contract identities exactly.
    pub owner_app: String,
    /// The confined project schema containing the renamed table.
    pub project_schema: String,
    /// The migrator role used for the approval-gated cleanup DDL. Optional.
    pub migrator_role: Option<String>,
    /// The pending-contract key returned by apply or status.
    pub pending_version: String,
    /// `"apply"` keeps the new column; `"abort"` keeps the old column.
    pub action: String,
    /// Explicit approval for the destructive column drop.
    pub approved: bool,
    /// Audit label recorded with the cleanup and resolution.
    pub applied_by: String,
    /// Ordered policy charter documents (TOML), starting with the root bound and
    /// used by the executor's guard.
    pub charter_layers: Vec<String>,
}

/// The typed request for the `history` verb.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct HistoryRequest {
    /// The project id the `ExecutorConfig` carries.
    pub project_id: String,
    /// The confined project schema.
    pub project_schema: String,
    /// Ordered policy charter documents (TOML), starting with the root bound and
    /// used by the executor's guard.
    pub charter_layers: Vec<String>,
}

/// The typed reply for `applyIr` (the projected [`ApplyOutcome`]).
///
/// [`ApplyOutcome`]: zero_migrate::apply::executor::ApplyOutcome
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct ApplyReply {
    /// Versions applied this run, in apply order.
    pub applied: Vec<String>,
    /// Versions already applied (skipped). Informational.
    pub skipped: Vec<String>,
    /// Versions recovered via the non-txn recovery path this run.
    pub recovered: Vec<String>,
    /// Outstanding PostgreSQL online-rename contracts after this operation.
    pub pending_contracts: Vec<ApplyPendingContractDto>,
}

/// How far a rollback should unwind, as a nested object rather than three flat
/// fields.
///
/// A `#[napi(object)]` cannot be a Rust enum carrying data, so the three shapes
/// share one struct and `kind` selects which operand applies. Keeping them nested
/// means a host reads one field to know what was asked for; flattening them into
/// the request would leave `version` and `steps` sitting next to unrelated deploy
/// inputs with nothing saying they are alternatives.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct RollbackTargetDto {
    /// `"toVersion"` unwinds everything applied AFTER the named version, keeping
    /// it; `"steps"` unwinds the n most recently applied; `"all"` unwinds
    /// everything. Required: there is no default, because every default would be
    /// a guess about how much of a schema to tear down.
    pub kind: String,
    /// The version to stop at. Only for `"toVersion"`.
    pub version: Option<String>,
    /// How many migrations to unwind. Only for `"steps"`.
    pub steps: Option<u32>,
}

/// The typed request for the `rollback` and `rollbackSqlite` verbs.
///
/// It carries the complete ordered envelope sequence rather than a prior/current
/// split: a rollback reconstructs the reverse SQL for migrations that are ALREADY
/// applied, so there is no "current" envelope to distinguish.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct RollbackRequest {
    /// The deploying app id (the `app_` prefixed id), stamped during guarded lowering so the
    /// reconstructed plan identities match the ones the deploy journaled.
    pub owner_app: String,
    /// The confined project schema the lower pins ops to.
    pub project_schema: String,
    /// The migrator role to `SET ROLE` under for the reverse DDL. Optional, and
    /// refused outright by `rollbackSqlite`: SQLite has no roles, so accepting one
    /// there would silently promise least-privilege that is not being applied.
    pub migrator_role: Option<String>,
    /// `"postgres" | "mysql"` for the host-driven verb, `"sqlite"` for the
    /// in-process one.
    pub dialect: String,
    /// The project's `{ table: owner_app }` ownership registry.
    pub registry: std::collections::HashMap<String, String>,
    /// The ordered authored migration envelopes, as real JavaScript values.
    pub envelopes: Vec<JsonValue>,
    /// Ordered policy charter documents (TOML), starting with the root bound. The
    /// guard over the reverse SQL is composed from these, so a `down` is held to
    /// the same bound its `up` was.
    pub charter_layers: Vec<String>,
    /// How far to unwind. Required.
    pub target: RollbackTargetDto,
    /// Whether the operator approved the teardown. A `down` is destructive by
    /// construction, so the engine refuses without it.
    pub approved: bool,
    /// Cross a migration that declares no `down` by skipping it rather than
    /// refusing. Honored only together with `backupAcknowledged`.
    pub force: bool,
    /// The operator's acknowledgement that a backup exists. Forcing past an
    /// irreversible migration discards data, so it takes both flags.
    pub backup_acknowledged: bool,
    /// The audit label recorded with the `rolled_back` events.
    pub applied_by: String,
}

/// The typed reply for `rollback` (the projected [`RollbackOutcome`]).
///
/// Deliberately NOT [`ApplyReply`]. The two verbs answer different questions, and
/// a shared shape would make every rollback carry three fields it can never fill
/// and every apply carry one it never fills. A host reading `applied` off a
/// rollback reply would read an empty list as "nothing happened".
///
/// [`RollbackOutcome`]: zero_migrate::RollbackOutcome
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct RollbackReply {
    /// Versions whose `down` ran and were journaled `rolled_back`, in the order
    /// they were unwound: reverse topological order of `depends_on`, so each
    /// version came down before anything it depends on.
    pub rolled_back: Vec<String>,
    /// Versions crossed WITHOUT running a `down`, because they declare none and
    /// the request carried both the force flag and the backup acknowledgement.
    /// Empty on every request that did not force.
    pub skipped_irreversible: Vec<String>,
    /// Operator-facing compatibility notices, including when a legacy applied
    /// row has NULL `down` and rollback reconstructed its reverse from source.
    pub advisories: Vec<String>,
}

/// One outstanding online-rename contract returned after apply.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct ApplyPendingContractDto {
    /// Table whose old and new columns currently coexist.
    pub table: String,
    /// Original column retained until the contract is completed.
    pub from_column: String,
    /// Destination column populated by the online expansion.
    pub to_column: String,
    /// Stable key accepted by the `resolvePending()` embedding API. The CLI
    /// discharges the same obligation by authored migration NAME, via
    /// `zero-migrate resolve`.
    pub pending_version: String,
}

/// The typed reply for `status` (the projected `MigrationStatus`).
///
/// **The `mig_…` ids below are LOGICAL PLAN ids, and they are a different namespace
/// from the journal versions [`ApplyReply::applied`] returns.** Both are spelled
/// `mig_…`, so a consumer that correlates the two gets no matches and no error.
/// Measured on live PostgreSQL: one `createTable` applied through the host path put
/// `mig_7n42DGM5RSBfCGYlS39M1y` in the journal and returned it from `apply`, while
/// `status` reported `applied: ["mig_7n42DGM5SrG4j3FrNuIVBe"]` and the same id as
/// `current_version` - correctly identifying the migration as applied, under an id
/// that appears in no journal row.
///
/// To correlate a status entry with the journal, match on the plan rather than on
/// the string: `plans[]` carries the per-plan detail, and `history` reads journal
/// identities directly.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct StatusReply {
    /// Versions whose UNWIND was interrupted and never resolved (MySQL only).
    ///
    /// `apply` and `rollback` both refuse over this state. Status reporting a
    /// clean project while they refuse is a contradiction the operator has to
    /// resolve with no information, so the versions travel here (F661).
    pub interrupted_unwinds: Vec<String>,
    /// The highest net-applied LOGICAL PLAN id (`mig_…`), or `None` when nothing is
    /// applied. NOT the journal version - see the type doc.
    pub current_version: Option<String>,
    /// Net-applied logical plan ids, in order. NOT journal versions - see the type
    /// doc.
    pub applied: Vec<String>,
    /// Supplied logical plan ids that are NOT net-applied, in apply order. NOT
    /// journal versions - see the type doc.
    pub pending: Vec<String>,
    /// Supplied logical plans whose online rename was explicitly aborted.
    pub aborted: Vec<String>,
    /// Versions whose latest event is a rollback, in version order.
    pub rolled_back: Vec<String>,
    /// Outstanding online-contract obligations, including orphan diagnosis.
    pub pending_contracts: Vec<PendingContractStatusDto>,
    /// Plans blocked by an outstanding dependency contract.
    pub blocked: Vec<BlockedPlanDto>,
    /// Completed or inflight journal identities absent from supplied plans.
    pub unexpected_journal: Vec<UnexpectedJournalEntryDto>,
    /// Plan-aware detail. Absent on the legacy migration-only `status` verb;
    /// present (including as an empty array) on `statusIr`.
    pub plans: Option<Vec<PlanStatusDto>>,
    /// True when a peer's deploy held the project lock, so this reply carries NO
    /// reconciled state: every other field is empty because no catalog or journal
    /// read ran. Always present, so a CI that wants contention to fail can opt in
    /// by branching on it instead of parsing a message.
    pub busy: bool,
    /// Sessions reported holding the project lock. Empty unless `busy`.
    pub lock_holders: Vec<ProjectLockHolderDto>,
}

/// One session reported holding the project lock in a busy status reply.
///
/// Carries no duration: `pg_locks` records no acquisition time, so any duration
/// here would age the holder's session or statement rather than the lock.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct ProjectLockHolderDto {
    /// Backend process id of the holding session.
    pub pid: i64,
    /// The holder's `application_name`, when it set one.
    pub application_name: Option<String>,
    /// The holder's activity state (`active`, `idle in transaction`, ...).
    pub state: Option<String>,
    /// The holder's current statement. Absent unless the reading role may see
    /// other sessions' statement text.
    pub query: Option<String>,
}

/// One outstanding online-contract obligation in a status reply.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct PendingContractStatusDto {
    /// Table whose deferred contract remains outstanding.
    pub table: String,
    /// Stable obligation key used by operator resolution.
    pub pending_version: String,
    /// Whether the supplying plan is absent from the current manifest set.
    pub orphaned: bool,
    /// The exact table-interlock refusal `apply` prints for this obligation.
    pub reason: Option<String>,
}

/// One plan blocked by an outstanding dependency contract.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct BlockedPlanDto {
    /// Logical id of the blocked plan.
    pub blocked: String,
    /// Logical id of the dependency plan.
    pub dependency: String,
    /// Outstanding obligation key on the dependency.
    pub pending_version: String,
    /// The exact dependency refusal `apply` prints for this blocked plan.
    pub reason: Option<String>,
}

/// One journal identity absent from every supplied plan manifest.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct UnexpectedJournalEntryDto {
    /// Unexpected step identity.
    pub version: String,
    /// `applied` or `inflight`.
    pub state: String,
    /// Journal checksum retained for diagnosis.
    pub journal_checksum: String,
    /// Journaled kind when the entry is completed; absent for inflight markers.
    pub journal_kind: Option<String>,
}

/// One plan in a plan-aware status reply.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct PlanStatusDto {
    /// Stable logical plan id.
    pub version: String,
    /// Authored migration name.
    pub name: String,
    /// `applied | pending | partial | drifted | blocked | unknownDependency`.
    pub state: String,
    /// Ordered journal-visible steps.
    pub steps: Vec<PlanStatusStepDto>,
    /// Dependencies omitted from the supplied plan set.
    pub missing_dependencies: Vec<String>,
    /// The authoritative table set apply uses for the pending-contract interlock.
    /// Present on plan-aware IR status; absent from legacy/core-only projections.
    pub touched_tables: Option<Vec<String>>,
}

/// One executable step in a plan-aware status reply.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct PlanStatusStepDto {
    /// Stable step journal id.
    pub version: String,
    /// Human-readable operation label.
    pub name: String,
    /// `ddl | dml | backfill | synchronizeIdentity | onlineExpand |
    /// onlineContract | sqliteRebuild`.
    pub kind: String,
    /// `pending | inflight | applied | drifted`.
    pub state: String,
    /// `guardUpdates | externalInvariant` for resumable backfills.
    pub cursor_stability_mode: Option<String>,
    /// The explicitly approved invariant name for `externalInvariant`.
    pub cursor_stability_invariant: Option<String>,
    /// Named assertion that identity-allocating writes are quiesced.
    pub writes_quiesced: Option<String>,
}

/// One event in the typed `history` audit trail.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct HistoryEventDto {
    /// The shared monotonic sequence number — an `int8` audit key, so it crosses as
    /// a JS `bigint` (napi6) to survive a large sequence without float rounding.
    pub event_seq: BigInt,
    /// The migration version (`mig_…`).
    pub version: String,
    /// The migration name recorded on the event.
    pub name: String,
    /// `"applied" | "rolled_back"` — the projected `HistoryKind`.
    pub kind: String,
    /// The event timestamp (RFC-3339 / ISO-8601).
    pub at: String,
    /// The actor who performed the event.
    pub applied_by: String,
    /// The checksum recorded on the event.
    pub checksum: String,
}

/// The typed reply for `history` (the projected `Vec<HistoryEvent>`).
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct HistoryReply {
    /// The audit events, in `event_seq` order.
    pub events: Vec<HistoryEventDto>,
}

// ---------------------------------------------------------------------------
// `genArtifacts` — the sync, DB-free schema-artifact emitter verb.
// ---------------------------------------------------------------------------

/// The typed reply for `genArtifacts`: the two CO-EMITTED artifact strings.
///
/// napi-neutral (plain string/option fields) so [`crate::api`] builds it on the
/// napi-free test path too. `ok=false` + `error` on a fold/produce failure (never a
/// throw).
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct GenArtifactsReply {
    /// `true` iff both artifacts rendered cleanly.
    pub ok: bool,
    /// The generated `env.db.ts` source: a passive schema map built with the
    /// current `zero-migrate` authoring API and checked as `CreateTableArgs`.
    /// `None` on failure.
    pub env_db_ts: Option<String>,
    /// The `schema.runtime.json` bytes (the v1 `RuntimeSchemaDescriptor`, pretty +
    /// trailing newline). `None` on failure.
    pub runtime_json: Option<String>,
    /// A human-readable error when `ok == false`.
    pub error: Option<String>,
    /// Did the folded history carry an op-level `dialect()` wrapper?
    ///
    /// [`GenArtifactsSource::dialect`] is required and has no default because leg
    /// selection changes WHICH COLUMNS EXIST. This reports whether that mechanism
    /// was in play, so a host that supplies a constant can assert the constant was
    /// harmless instead of hoping. ABSENT (`undefined` to a JS caller, `None` in
    /// Rust) when `ok == false`: a refused call folded nothing and has no answer.
    /// A consumer asserting on this must therefore test `=== false` rather than
    /// falsiness, so neither a refusal nor an older addon that predates the field
    /// can pass as a clean result.
    ///
    /// Reports PRESENCE, not selection. A pg-only wrapper folded under SQLite
    /// selects no leg and no `default`, contributing nothing while the fold
    /// succeeds - the case a caller most needs told about - so a selection-shaped
    /// answer would be `false` exactly there. The descriptor source cannot carry a
    /// wrapper at all and reports `false` by construction.
    ///
    /// `false` does NOT mean the artifacts are dialect-independent. The
    /// materialized enum/domain capability gates and the identity/primary-key reuse
    /// rules key on the dialect too (see the `render_artifacts` contract), so this
    /// answers one narrow question and promises nothing wider.
    pub has_dialectal_ops: Option<bool>,
    /// **The structured export.** The folded schema as TYPED collections, in the same
    /// `CollectionDescriptorDto` vocabulary the manual source accepts — so a host can
    /// render its own artifacts instead of re-parsing `runtime_json` back out of the
    /// string this reply also carries.
    ///
    /// Collections are in name order (the engine returns a `BTreeMap`), which is the
    /// same order `runtime_json` serializes them in.
    ///
    /// ABSENT (`undefined` / `None`) when `ok == false`, for the reason
    /// `has_dialectal_ops` documents: a refused call folded nothing and has no answer.
    /// A consumer must therefore test for presence rather than for an empty array — a
    /// schema with no collections is a legitimate `[]`, and an older addon that
    /// predates this field is `undefined`, and those two must not read alike.
    ///
    /// WHAT IT DOES NOT CARRY: enough to rebuild `env_db_ts`. That file is rendered
    /// from the richer authoring IR precisely because the `FieldDef` vocabulary these
    /// descriptors speak collapses physical widths, value formats and keys. This is
    /// the runtime-descriptor half of the artifacts, not both halves.
    pub collections: Option<Vec<CollectionDescriptorDto>>,
    /// The dialect the fold ACTUALLY ran under (`"postgres" | "mysql" | "sqlite"`),
    /// echoed into the payload rather than left to the caller's memory of what it
    /// sent.
    ///
    /// `Op::Dialectal` leg selection changes WHICH COLUMNS EXIST, so `collections`
    /// is meaningless without the dialect it was folded under, and a consumer that
    /// stores or forwards the export must carry the two together. The engine
    /// normalises dialect spellings (`preview_dialect` accepts more than one name per
    /// target), so this is the resolved target, not the string that was passed in.
    ///
    /// ABSENT when `ok == false` — a refusal never reached a fold, and an unknown
    /// dialect spelling is one of the ways to be refused, so echoing the input there
    /// would report a target that was rejected.
    pub dialect: Option<String>,
}

/// One field of a collection — the `FieldDescriptor` mirror, in BOTH directions.
///
/// Mirrors the `@zeroship/db` wire `FieldDef` shape the manual evaluator produces.
/// The common facets are typed scalars; the rich sub-object facets (`encrypted`,
/// `mask`, `generated`, `identity`) cross as REAL JS values ([`JsonValue`]) and
/// deserialize into the engine `FieldDescriptor` verbatim.
///
/// It was the MANUAL SOURCE only, and the four fields added since say what that cost:
/// while nothing ever CONSTRUCTED one, a slot the producer defaults or ignores is
/// indistinguishable from a slot nothing needs. Carrying an export outward is what
/// told `reference_column`, `reference_name`, `char_len` and `max_length` apart from
/// the facets genuinely absent by design — see
/// `tests/collection_export_round_trip.rs`, which was RED on all four.
///
/// napi-NEUTRAL (`cfg_attr`), like the reply envelopes and for the same reason: it is
/// no longer inbound-only. `crate::api` builds one on the OUTBOUND export path, and
/// the napi-free `--no-default-features` build is what tests it.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct FieldDescriptorDto {
    /// The field (column) name.
    pub name: String,
    /// The DSL type token (`string` | `number` | `boolean` | `date` |
    /// `calendarDate` | `json` | `ref` | `bytes` | `id` | `vector` | …).
    #[cfg_attr(feature = "napi", napi(js_name = "type"))]
    pub ty: String,
    /// `true` ⇒ `NOT NULL`.
    pub required: Option<bool>,
    /// `true` ⇒ a unique index over this column.
    pub unique: Option<bool>,
    /// For a `ref` field, the referenced collection (FK target table).
    pub references: Option<String>,
    /// The referenced target COLUMN. A legacy declarative `ref` omits it and keeps its
    /// historical `id` target; a typed migration reference always records it.
    ///
    /// Added when this type stopped being inbound-only. It was absent while the DTO
    /// only ever fed the producer — which defaults it to `id` — and that absence
    /// became an export loss the moment the fold, which recovers the REAL target
    /// column from the FK it holds, had to hand its answer outward.
    pub reference_column: Option<String>,
    /// An explicit foreign-key constraint name. Absent references use the shared
    /// `<table>_<field>_fkey` derivation. Present for the same reason as
    /// `reference_column`: the fold knows the real name, and dropping it here made an
    /// exported FK indistinguishable from a derived-name one.
    pub reference_name: Option<String>,
    /// `ref` ON DELETE policy (`cascade`|`restrict`|`setNull`|`setDefault`|`noAction`).
    pub on_delete: Option<String>,
    /// `ref` ON UPDATE policy.
    pub on_update: Option<String>,
    /// Whether the FK is `DEFERRABLE INITIALLY DEFERRED`.
    pub deferrable: Option<bool>,
    /// Column `DEFAULT` value.
    pub default: Option<JsonValue>,
    /// Numeric `min` bound (lifts a CHECK).
    pub min: Option<f64>,
    /// Numeric `max` bound (lifts a CHECK).
    pub max: Option<f64>,
    /// Enum membership (string or numeric members).
    #[cfg_attr(feature = "napi", napi(js_name = "enum"))]
    pub enum_values: Option<Vec<JsonValue>>,
    /// A legacy internal `<prefix>_<22 base62 UUIDv7>` platform-ID prefix.
    pub id_prefix: Option<String>,
    /// A `t.vector(dims, …)` dimensionality.
    pub vector_dims: Option<i64>,
    /// `t.char(len)` — the FIXED width of a `CHAR(N)` column.
    pub char_len: Option<i64>,
    /// `t.string({ length })` — the BOUND on a `VARCHAR(N)` column.
    ///
    /// Crosses in BOTH directions, and the inbound half is load-bearing rather than
    /// decorative: `string` is a TWO-type token (`ColType::String { length }` and
    /// `ColType::Text` both spell it), so `token_to_col_type` reads this value to pick
    /// between them. It used to ignore it, and a consumer that exported a
    /// `VARCHAR(64)` and fed it back as a manual source got an unbounded `TEXT` — a
    /// column PostgreSQL then stored a 200-character value in
    /// (`zero-migrate/tests/fold_live/pg_bounded_string_producer_live.rs`).
    /// Pinned end to end by `tests/collection_export_round_trip.rs`.
    pub max_length: Option<i64>,
    /// `t.numeric({ precision, scale })` — total digits of a FIXED-PRECISION decimal,
    /// with [`Self::scale`] beside it.
    ///
    /// Unlike the three widths above, this pair does not merely PARAMETERISE the type
    /// token - it decides which type the token means. `number` is what both
    /// `ColType::Double` and `ColType::Decimal` are spelled as, so a wire that dropped
    /// these two would export a `t.numeric(20, 4)` column as a float, and a host
    /// feeding it back would get one. It crosses in BOTH directions for that reason
    /// (`token_to_col_type` reads `precision` to pick the `ColType`), exactly as
    /// `max_length` does for the other two-type token, `string`.
    pub precision: Option<i64>,
    /// `t.numeric({ precision, scale })` — digits after the point. See
    /// [`Self::precision`].
    pub scale: Option<i64>,
    /// A `t.vector(_, { metric })` distance metric (`cosine`|`l2`|`innerProduct`).
    pub vector_metric: Option<String>,
    /// `t.string({ caseSensitive: false })` — only `Some(false)` is meaningful.
    pub case_sensitive: Option<bool>,
    /// The `t.encrypted({ mode, keyId, wraps })` sub-object (verbatim).
    pub encrypted: Option<JsonValue>,
    /// The `.mask({ kind, classification })` sub-object (verbatim).
    pub mask: Option<JsonValue>,
    /// A generated/computed column facet (structured IR, never raw SQL).
    pub generated: Option<JsonValue>,
    /// A SQL identity column facet.
    pub identity: Option<JsonValue>,
}

/// One declared named index of a collection (the `_indexes` array entry).
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct IndexDescriptorDto {
    /// The index name (already collision-stable from the SDK).
    pub name: String,
    /// The columns the index covers, in order.
    pub columns: Vec<String>,
    /// `true` ⇒ a unique index.
    pub unique: Option<bool>,
}

/// Per-collection runtime options that do not round-trip through catalog state.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct RuntimeOptionsDto {
    /// `schema(...).softDelete()`.
    pub soft_delete: Option<bool>,
    /// `schema(...).withVersioning()`.
    pub versioning: Option<bool>,
    /// `schema(...).strictness(...)` — `"strict"` | `"lenient"` | `"off"`. Default
    /// (absent) is `"strict"`.
    pub strictness: Option<String>,
}

/// One collection (table) — the `CollectionDescriptor` mirror, in BOTH directions.
///
/// INBOUND it is the manual `genArtifacts` source. OUTBOUND it is what
/// [`GenArtifactsReply::collections`] carries, so a host can read the folded schema as
/// structure and render its own files. The two directions use ONE type on purpose:
/// anything the export emits is something the manual source accepts.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct CollectionDescriptorDto {
    /// The collection (table) name.
    pub name: String,
    /// The declaring app id (`app_…`). The migrate producer stamps ownership from it.
    pub owner_app: String,
    /// The author-declared fields. Columns owned by the active policy are supplied
    /// by the producer's explicit policy-resolution pass.
    pub fields: Vec<FieldDescriptorDto>,
    /// The declared named indexes.
    pub indexes: Option<Vec<IndexDescriptorDto>>,
    /// Per-collection runtime options (`softDelete`/`versioning`/`strictness`).
    pub runtime_options: Option<RuntimeOptionsDto>,
}

/// The tagged SOURCE for `genArtifacts` — EITHER IR envelopes (the generated
/// source) OR a declared descriptor set (the manual source). A `#[napi(object)]`
/// cannot be a Rust enum with data, so the two arms are optional fields; exactly one
/// must be populated. Both arms funnel through the SAME Rust renderer, so their
/// output is byte-identical for equivalent schemas.
///
/// `dialect` and `charterLayers` are both REQUIRED. A caller that goes through these
/// types never has to think about that, because omitting either is a compile error
/// that names every one it missed. A caller that skips them learns it one field at a
/// time: the object is deserialized field by field in the order declared below, so an
/// absent field reports "Missing field `dialect`" and stops, naming neither this
/// interface nor the verb, and a second absent field is never reached. A field that is
/// PRESENT but wrong-typed reports better - that path names the interface and the
/// field, as in "on GenArtifactsSource.dialect".
///
/// Nothing on this side can improve the absent-field message. The object is
/// deserialized before `gen_artifacts` is entered, so no check the body performs can
/// see the raw object, and the absent-field error is never handed the interface name
/// to begin with.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct GenArtifactsSource {
    /// The GENERATED source: a set of IR envelopes (`{ ir_version, name, ops }`), in
    /// version order. Their `ops` are concatenated and folded. Mutually exclusive
    /// with `descriptors`.
    pub envelopes: Option<Vec<JsonValue>>,
    /// The MANUAL source: a declared `CollectionDescriptor` set. Turned into
    /// `createTable` ops via the producer, then folded through the same tail.
    /// Mutually exclusive with `envelopes`.
    pub descriptors: Option<Vec<CollectionDescriptorDto>>,
    /// The project's REAL target dialect: `"postgres" | "sqlite" | "mysql"`.
    ///
    /// REQUIRED, with no default. The fold selects `Op::Dialectal` legs, so a
    /// history authored with `dialect({ pg, mysql })` yields a different column set
    /// per target; generating a MySQL project's artifacts under Postgres names
    /// columns its database does not have. Making the field optional would put that
    /// mistake back within reach, so omitting it is a type error rather than a
    /// silent Postgres fallback. This does not cover choosing the dialect for the
    /// caller: there is no URL or config in scope here, so the caller owns it. The
    /// only in-repo call site is
    /// `packages/zero-migrate-cli/tests/host/gen-artifacts-dialect.test.ts`, which
    /// names the dialect explicitly per arm; no shipped code path resolves one for
    /// this verb, so the `DriverConfig`-to-dialect mapping `apply`/`status` use
    /// (`packages/zero-migrate-cli/src/index.ts` `dialectOf`) does NOT reach it.
    pub dialect: String,
    /// The project schema the fold threads (FK `definition`s embed it). Optional;
    /// defaults to `"public"`.
    pub project_schema: Option<String>,
    /// The **policy input**: ordered charter documents (TOML) that drive the
    /// confined system-shape injection. The first document is the root bound and
    /// each subsequent document is an untrusted narrowing layer. The composed
    /// policy is applied identically on the envelope and descriptor sides.
    pub charter_layers: Vec<String>,
}

/// The source and rendering context for the sync, DB-free `previewSql` verb.
///
/// Each `envelopes` entry is a complete IR envelope serialized as JSON. The addon
/// composes `charter_layers` once, then renders every envelope independently in
/// input order for the requested dialect.
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct PreviewSqlSource {
    /// Complete IR envelope JSON documents, in migration order.
    pub envelopes: Vec<String>,
    /// The target SQL dialect: `"postgres" | "sqlite" | "mysql"`.
    pub dialect: String,
    /// Schema used for operations that omit an explicit qualifier.
    pub default_schema: String,
    /// App attribution stamped into the offline preview lowering context.
    pub owner_app: String,
    /// Ordered policy-charter TOML documents (root bound, then narrowing layers).
    pub charter_layers: Vec<String>,
}

/// The typed reply for the sync, DB-free `loadVerify` gate. Never throws for a
/// malformed document; `ok=false` + a message.
///
/// napi-neutral (plain scalar fields) so [`crate::api`] builds it on the napi-free
/// test path too.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct LoadVerifyReply {
    /// `true` iff the IR document loaded + validated cleanly.
    pub ok: bool,
    /// The document's `ir_version` (when it deserialized far enough to read it).
    pub ir_version: Option<u32>,
    /// The number of `op.*` operations in the document (when loaded).
    pub op_count: Option<u32>,
    /// A human-readable error when `ok == false` (the first fail-closed rejection).
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// `buildInfo` - the addon's build identity.
// ---------------------------------------------------------------------------

/// The build identity of the loaded addon, so a host that resolved a `.node` by
/// path can log WHICH artifact it got. Every field is derived from committed
/// bytes at compile time, so rebuilding an unchanged tree reports the same
/// values.
///
/// napi-neutral (plain scalar fields) so [`crate::api`] builds it on the
/// napi-free test path too.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct BuildInfo {
    /// The crate version (workspace `[workspace.package] version`). Distinguishes
    /// releases, NOT two builds of the same unreleased version.
    pub version: String,
    /// The IR-format floor this addon was built against - the same number
    /// `irVersion()` returns, repeated so one call answers the whole identity.
    pub ir_version: u32,
    /// Lowercase 64-char sha256 over the workspace manifests, `Cargo.lock`, and
    /// every `crates/*/src` file. This is what tells a pre-fix artifact from a
    /// post-fix one when the version has not moved. It does NOT cover the JS
    /// packages, the rustc version, the cargo profile, or the enabled features -
    /// and NOTHING ELSE IN THIS REPLY COVERS THEM EITHER. The only other fields are
    /// `version` (which moves on a release, not on a rebuild) and `ir_version` (a
    /// format floor), so two `.node` artifacts built from the same Rust sources
    /// under a different toolchain, profile, or feature set report an identical
    /// identity here. A hole, not a handoff.
    pub source_digest: String,
}

/// One operational advisory the analyzer raised, with the statement that raised
/// it.
///
/// The statement travels with the advisory on purpose. The analyzer's message
/// names the CONSTRAINT (`lk_t_e_key`) but not the TABLE, and an operator
/// deciding whether to take a lock needs to know which table is going to be
/// locked. Advisories never gate; this is information, not a verdict.
#[cfg_attr(feature = "napi", napi(object))]
#[derive(Debug, Clone)]
pub struct AdvisoryDto {
    /// The migration whose lowered DDL raised it.
    pub migration: String,
    /// The stable analyzer rule id.
    pub rule: String,
    /// `"notice" | "warning" | ...` — the analyzer's severity, lowercased.
    pub severity: String,
    /// What the operational risk is.
    pub message: String,
    /// The safer alternative, when the analyzer knows one.
    pub suggestion: Option<String>,
    /// The exact statement the advisory describes.
    pub statement: String,
}
