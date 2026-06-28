//! The PR4a §8.9.2 HOSTED recorder HTTP contract: `POST /v1/recorder/record`.
//!
//! Transport-agnostic by design — this module owns the REQUEST/RESPONSE DTOs, the
//! §8.8 structured-error mapping, and the `handle_record` function that drives a
//! [`RecorderService`] from a parsed request + the bearer token. The actual route
//! mounting (ntex/axum/whatever the hosting binary uses) is a thin shell PR4 adds;
//! keeping the contract here makes it unit-testable without a live HTTP stack and
//! keeps the lean transport choice out of the migrate-js crate's dep graph.
//!
//! ## Contract (design §8.9.2)
//!
//! `POST /v1/recorder/record`
//!   Authorization: Bearer <PAT | ZEROSHIP_TOKEN>
//!   Body: { ts_source, app_id, schema_types_blob? }
//!   200 -> { ir_json, ts_provenance_blob, checksum }
//!   4xx/5xx -> the §8.8 structured error { code, message, ... }
//!
//! The `app_id` is cross-checked against the token's owned apps SERVER-SIDE (§8.6).
//! A fresh sandbox child is spawned per call (no pooling). Recorder-unreachable is a
//! 503 the client retries / falls back to local recording (§8.9.2) — NOT a build
//! failure.

use serde::{Deserialize, Serialize};

pub use super::recorder_protocol::MAX_TS_SOURCE_BYTES;
use super::recorder_service::{RecorderError, RecorderService};

/// Max accepted client-controlled migration `name` (PR4a code-critic MED #5). A
/// filename-derived label is short; cap it so an oversized name cannot reach the
/// child's `v8::String::new` (which returns None / would otherwise be a panic vector).
pub const MAX_NAME_BYTES: usize = 4 * 1024;

/// Max accepted client-controlled `app_id` (PR4a code-critic LOW — bound app_id at the
/// HTTP boundary, symmetric with the existing `ts_source`/`name` guards). A typed_id
/// (`app_…` base62 UUIDv7) is a few dozen bytes; cap it small so a hostile oversized
/// id is rejected with 413 BEFORE `svc.record()` spawns a child or it reaches the
/// authorizer / `v8::String` paths.
pub const MAX_APP_ID_BYTES: usize = 256;

/// Max accepted type-only `schema_types_blob` (PR4a code-critic LOW — bound the blob at
/// the HTTP boundary). It is delivered IN-MEMORY to the child and exposed as a read-only
/// `globalThis.__zsSchemaTypes` via `v8::String::new` (a panic vector above V8's ~512MB
/// limit). A type-only `.d.ts`-style blob is small; we cap at 8 MiB (same generous
/// ceiling as `ts_source`), rejecting a hostile blob with 413 before a child is spawned.
pub const MAX_SCHEMA_TYPES_BYTES: usize = MAX_TS_SOURCE_BYTES;

/// The `POST /v1/recorder/record` request body (design §8.9.2).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecordHttpRequest {
    /// The untrusted migration `.ts` source (bundled, self-contained).
    pub ts_source: String,
    /// The claimed owner app — cross-checked against the token server-side (§8.6).
    pub app_id: String,
    /// The optional type-only schema-types blob (read-only context for the recorder).
    /// Delivered IN-MEMORY to the sandbox child and exposed to the recorder scope as a
    /// read-only `globalThis.__zsSchemaTypes` (no fs read needed). The wire DTO and the
    /// actual recorder input are in lock-step — no contract drift (PR4a code-critic
    /// LOW #7).
    #[serde(default)]
    pub schema_types_blob: Option<String>,
    /// The filename-derived migration name (optional; the module's own `name` wins).
    #[serde(default)]
    pub name: Option<String>,
}

/// The 200 response body (design §8.9.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordHttpResponse {
    /// The recorded `.ir.json` envelope string.
    pub ir_json: String,
    /// The `.ts` provenance blob (the source the §5.1 deploy-time provenance gate
    /// re-records + checksum-matches). It is the exact `ts_source` the recorder
    /// evaluated, so the deploy gate re-runs the SAME bytes.
    pub ts_provenance_blob: String,
    /// The typed-value checksum of the recorded IR (`Checksum::of_ir`), hex.
    pub checksum: String,
}

/// The §8.8 structured-error envelope (machine-readable; `suggested_fix` leads in the
/// human rendering).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredError {
    /// The §8.8 machine-readable code.
    pub code: String,
    /// The human-facing message (the projection).
    pub message: String,
    /// The HTTP status the transport should return.
    pub http_status: u16,
    /// Whether the client should retry / fall back to local recording (recorder
    /// unreachable / overloaded) — NOT a build failure (§8.9.2).
    pub retryable: bool,
}

impl From<&RecorderError> for StructuredError {
    fn from(e: &RecorderError) -> Self {
        let (http_status, retryable) = match e {
            RecorderError::Unauthorized(_) => (403, false),
            RecorderError::EvalError(_) => (422, false), // authoring reject
            RecorderError::BudgetExceeded { .. } => (422, false), // bounded build
            RecorderError::KilledBySeccomp => (422, false), // the migration tried something denied
            RecorderError::RequestTooLarge { .. } => (413, false),
            RecorderError::SandboxRefused(_) => (503, true), // environment refusal
            RecorderError::EnvironmentKilled(_) => (503, true), // ambiguous bare SIGKILL -> retry/fallback-to-local
            RecorderError::Overloaded => (429, true),        // backpressure -> retry/queue
            RecorderError::Spawn(_) => (503, true),          // recorder-unreachable -> fallback-to-local
        };
        StructuredError {
            code: e.code().to_string(),
            message: e.to_string(),
            http_status,
            retryable,
        }
    }
}

/// The result of handling a `record` request: either the 200 body or a structured
/// error with an HTTP status. The transport shell maps this to the wire.
#[derive(Debug, Clone)]
pub enum RecordHttpOutcome {
    Ok(RecordHttpResponse),
    Err(StructuredError),
}

/// Handle a `POST /v1/recorder/record` (design §8.9.2), transport-agnostically.
///
/// `bearer` is the PAT/`ZEROSHIP_TOKEN` from the `Authorization: Bearer …` header
/// (or `None` if absent — a 401-class refusal). `svc` carries the authorizer +
/// sandbox + limits. On success the response carries the `.ir.json`, the `.ts`
/// provenance blob, and the typed-value checksum.
pub fn handle_record(
    svc: &RecorderService,
    bearer: Option<&str>,
    req: &RecordHttpRequest,
) -> RecordHttpOutcome {
    let token = match bearer {
        Some(t) if !t.is_empty() => t,
        _ => {
            return RecordHttpOutcome::Err(StructuredError {
                code: "RECORDER_UNAUTHORIZED".into(),
                message: "missing bearer token".into(),
                http_status: 401,
                retryable: false,
            })
        }
    };

    // ---- Input bounds (PR4a code-critic MED #5) ----
    // Reject hostile oversized client-controlled inputs BEFORE spawning a sandbox
    // child / reaching the child's `v8::String::new` (a >512MB string is a panic
    // vector; an unbounded blob is DoS amplification). 413 Payload Too Large.
    if req.ts_source.len() > MAX_TS_SOURCE_BYTES {
        return RecordHttpOutcome::Err(StructuredError {
            code: "RECORDER_REQUEST_TOO_LARGE".into(),
            message: format!(
                "ts_source is {} bytes; the recorder accepts at most {} bytes",
                req.ts_source.len(),
                MAX_TS_SOURCE_BYTES
            ),
            http_status: 413,
            retryable: false,
        });
    }
    if let Some(n) = req.name.as_deref() {
        if n.len() > MAX_NAME_BYTES {
            return RecordHttpOutcome::Err(StructuredError {
                code: "RECORDER_REQUEST_TOO_LARGE".into(),
                message: format!(
                    "name is {} bytes; the recorder accepts at most {} bytes",
                    n.len(),
                    MAX_NAME_BYTES
                ),
                http_status: 413,
                retryable: false,
            });
        }
    }
    // app_id + schema_types_blob are ALSO client-controlled; bound them symmetrically
    // with ts_source/name (PR4a code-critic LOW). Reject with 413 BEFORE svc.record()
    // spawns a child (app_id reaches the authorizer; the blob reaches the child's
    // `v8::String::new`, a panic vector above V8's max string length).
    if req.app_id.len() > MAX_APP_ID_BYTES {
        return RecordHttpOutcome::Err(StructuredError {
            code: "RECORDER_REQUEST_TOO_LARGE".into(),
            message: format!(
                "app_id is {} bytes; the recorder accepts at most {} bytes",
                req.app_id.len(),
                MAX_APP_ID_BYTES
            ),
            http_status: 413,
            retryable: false,
        });
    }
    if let Some(blob) = req.schema_types_blob.as_deref() {
        if blob.len() > MAX_SCHEMA_TYPES_BYTES {
            return RecordHttpOutcome::Err(StructuredError {
                code: "RECORDER_REQUEST_TOO_LARGE".into(),
                message: format!(
                    "schema_types_blob is {} bytes; the recorder accepts at most {} bytes",
                    blob.len(),
                    MAX_SCHEMA_TYPES_BYTES
                ),
                http_status: 413,
                retryable: false,
            });
        }
    }

    let name = req.name.as_deref().unwrap_or("migration");
    match svc.record(
        token,
        &req.app_id,
        &req.ts_source,
        name,
        req.schema_types_blob.as_deref(),
    ) {
        Ok(result) => {
            // Fold the single authoritative typed-value checksum over the recorded IR
            // (§2.4 point 2: the JS side emits ops; Rust folds the one checksum).
            let checksum = match checksum_of_ir_envelope(&result.ir_json) {
                Ok(c) => c,
                Err(e) => {
                    return RecordHttpOutcome::Err(StructuredError {
                        code: "RECORD_EVAL_ERROR".into(),
                        message: format!("recorded IR did not re-parse for checksum: {e}"),
                        http_status: 422,
                        retryable: false,
                    })
                }
            };
            RecordHttpOutcome::Ok(RecordHttpResponse {
                // The deploy-time provenance gate re-records THIS exact source.
                ts_provenance_blob: req.ts_source.clone(),
                ir_json: result.ir_json,
                checksum,
            })
        }
        Err(e) => RecordHttpOutcome::Err((&e).into()),
    }
}

/// Fold the typed-value checksum (`Checksum::of_ir`) over the recorder's `.ir.json`
/// ENVELOPE string. The envelope is `{ ok, ir: { ir_version, name, ops, owner_app? } }`
/// (the `op_recorder.js` shape) — we re-parse it through the real `MigrationIr` and
/// compute the one authoritative checksum, identical to the round-trip gate.
fn checksum_of_ir_envelope(ir_json: &str) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Env {
        #[serde(default)]
        ir: Option<serde_json::Value>,
    }
    use crate::model::ir::CanonicalOpList;
    use crate::{Checksum, MigrationFlags, MigrationIr};

    let env: Env = serde_json::from_str(ir_json).map_err(|e| e.to_string())?;
    let ir_value = env.ir.ok_or_else(|| "envelope missing `ir`".to_string())?;
    let bytes = serde_json::to_string(&ir_value).map_err(|e| e.to_string())?;
    let ir: MigrationIr = serde_json::from_str(&bytes).map_err(|e| e.to_string())?;
    // The same authoritative anchor the §2.5 round-trip gate folds (PR1 default-flags
    // domain): op list + default flags + owner + preconditions, dialect-neutral.
    let checksum = Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    );
    Ok(checksum.as_str().to_string())
}
