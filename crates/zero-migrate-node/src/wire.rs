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
//!    [`HistoryRequest`]/[`ApplyReply`]/[`StatusReply`]/[`HistoryReply`]/
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

#[cfg(feature = "napi")]
use napi::bindgen_prelude::BigInt;
#[cfg(feature = "napi")]
use napi_derive::napi;

/// A `serde_json::Value` crossing the boundary as a REAL JS value (object/array/…),
/// enabled by napi's `serde-json` feature — the IR `ops` AST and a lowered
/// `Migration` ride this instead of a re-serialized JSON string.
#[cfg(feature = "napi")]
pub type JsonValue = serde_json::Value;

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
// 2. Typed verb request/response envelopes (napi-gated — the addon produces /
//    consumes them; they carry napi-only field types: `JsonValue`, `BigInt`).
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
    /// `"postgres" | "mysql"` — selects the dialect backend (SQLite is in-process).
    pub dialect: String,
    /// The project's `{ table: owner_app }` ownership registry. Empty on a
    /// fresh single-app project.
    pub registry: std::collections::HashMap<String, String>,
    /// The pure-JS `.ir.json` envelope `{ ir_version, name, ops }` as a JS value.
    pub envelope: JsonValue,
    /// Whether destructive changes are pre-approved.
    pub approved: bool,
    /// The audit `applied_by` label recorded in the journal.
    pub applied_by: String,
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
    /// The pre-lowered migrations to reconcile against the journal, each a
    /// `Migration` as a JS value (empty array for the read/empty-journal flow).
    pub migrations: Vec<JsonValue>,
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
}

/// The typed reply for `applyIr` (the projected [`ApplyOutcome`]).
///
/// [`ApplyOutcome`]: zero_migrate::apply::executor::ApplyOutcome
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ApplyReply {
    /// Versions applied this run, in apply order.
    pub applied: Vec<String>,
    /// Versions already applied (skipped). Informational.
    pub skipped: Vec<String>,
    /// Versions recovered via the non-txn recovery path this run.
    pub recovered: Vec<String>,
}

/// The typed reply for `status` (the projected `MigrationStatus`).
#[cfg(feature = "napi")]
#[napi(object)]
#[derive(Debug, Clone)]
pub struct StatusReply {
    /// The highest net-applied version (`mig_…`), or `None` when nothing is applied.
    pub current_version: Option<String>,
    /// Net-applied versions, in version order.
    pub applied: Vec<String>,
    /// Supplied versions that are NOT net-applied, in apply order.
    pub pending: Vec<String>,
    /// Versions whose latest event is a rollback, in version order.
    pub rolled_back: Vec<String>,
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
