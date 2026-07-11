//! V8-backed `op.*` migration recording into typed migration IR.
//!
//! This module evaluates a creator migration module (which calls the
//! `@zeroship/migrate` `op.*` DSL inside `up()`) in zeroship-runtime's V8 sandbox
//! and records the emitted op list into a [`MigrationIr`]. Creator builds keep
//! that IR in memory; callers that need bytes canonicalize the value directly.
//!
//! Like [`super::eval`] this is one of the places V8 enters the migrate family, and
//! it reuses the runtime's existing sandbox + module loader (no second JS engine).
//! The recorder runs untrusted creator JS under the same sandbox parity as app
//! code (§8.9).

use crate::model::profile::PolicyProfile;
use crate::model::table_shape::{resolve_create_table_policy, TableShapeError};
use crate::runtime_host::{AuthoringHost, GlobalsProfile, HostEvalError};
use crate::MigrationIr;

use super::embedding::{module_graph, FrontendProgram};
use super::recorder_protocol::MAX_TS_SOURCE_BYTES;

/// An error from recording a migration module.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// V8 module loading / evaluation failed (syntax error, throw at module top
    /// level, unresolved import). Carries the runtime's error string.
    #[error("migration module evaluation failed: {0}")]
    V8(String),
    /// The adapter ran but reported a recording error (e.g. the module exports no
    /// `up()`, or an op-function threw).
    #[error("op recording failed: {0}")]
    Recording(String),
    /// The adapter left no `globalThis.__zsOpIR` (glue/runtime contract break).
    #[error("op recorder produced no IR (internal contract break)")]
    NoIr,
    /// The recorded envelope did not deserialize into a [`MigrationIr`] — the
    /// recorded ops violate the FROZEN wire contract (an out-of-domain scalar, an
    /// unknown op/expr node, a non-camelCase field). This is the gate biting: the
    /// JS builder may NOT emit a shape the Rust IR cannot represent (§2.5).
    #[error("recorded .ir.json does not match the frozen IR contract: {0}")]
    Contract(String),
    /// The source exceeded the shared recorder source cap.
    #[error("{field} is {actual} bytes; the recorder accepts at most {limit} bytes")]
    SourceTooLarge {
        /// The source field.
        field: &'static str,
        /// The accepted byte limit.
        limit: usize,
        /// The observed byte count.
        actual: usize,
    },
    /// The recorded IR violated the active profile's table-shape policy.
    #[error("recorded .ir.json violates active profile table shape: {0}")]
    TableShape(#[from] TableShapeError),
}

/// A recorded migration plus the determinism lint outcome.
///
/// Successful recordings may carry advisory source-lint warnings.
#[derive(Debug, Clone)]
pub struct RecordOutcome {
    /// The recorded, frozen-contract-validated migration IR.
    pub ir: MigrationIr,
    /// The §4.3 determinism findings surfaced on the migration source (may be empty).
    pub warnings: Vec<DeterminismFinding>,
}

/// The deserialized adapter result mirroring the JSON the glue emits on
/// `globalThis.__zsOpIR`.
#[derive(serde::Deserialize)]
struct OpIrEnvelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ir: Option<serde_json::Value>,
}

/// **UNSANDBOXED, in-process — for the test oracle / trusted-input use ONLY.**
///
/// Record a self-contained migration module's `up()` op list into a typed
/// [`MigrationIr`] (§2.5 — the JS half of the anti-drift gate).
///
/// # Safety — runs untrusted `.ts` in-process (the §8.9 threat)
///
/// This evaluates `migration_source` (untrusted creator / AI-authored JS) in an
/// IN-PROCESS V8 isolate with **no seccomp / landlock / network namespace** — the
/// exact §8.9 threat the kernel-sandboxed recorder child closes. The PR4 build/CLI
/// path NEVER calls this: it records via [`super::recorder_service::spawn_sandboxed_record`]
/// (the kernel-isolated child). This twin exists ONLY as the in-crate golden /
/// round-trip test oracle (`op_round_trip`, `full_surface`) and trusted-input
/// diagnostics. Production and CLI recording MUST go through
/// [`super::recorder_service::spawn_sandboxed_record`].
///
/// `owner_app` stamps the recorded `owner_app` HINT (the engine server-stamps the
/// authoritative one at deploy, §8.6). `name` is the filename-derived label used
/// when the module omits an explicit `name`.
///
/// Returns the recorded `MigrationIr` (already validated against the frozen wire
/// contract by `serde` — an out-of-contract op fails [`RecordError::Contract`]).
///
/// # Errors
/// See [`RecordError`].
#[doc(hidden)]
pub fn record_migration_to_ir_unsandboxed(
    host: &impl AuthoringHost,
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<MigrationIr, RecordError> {
    Ok(
        record_migration_to_ir_with_warnings_unsandboxed(host, migration_source, owner_app, name)?
            .ir,
    )
}

/// **UNSANDBOXED, in-process — for the test oracle / trusted-input use ONLY.**
/// See [`record_migration_to_ir_unsandboxed`]'s safety note (the §8.9
/// in-process-eval threat):
/// the PR4 build/CLI path records via the kernel-sandboxed child, never here.
///
/// This is the same record path as [`record_migration_to_ir_unsandboxed`], but it
/// keeps the historical [`RecordOutcome`] wrapper used by tests. The legacy regex
/// lint is an advisory warning source and never gates recording.
///
/// # Errors
/// See [`RecordError`].
#[doc(hidden)]
pub fn record_migration_to_ir_with_warnings_unsandboxed(
    host: &impl AuthoringHost,
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<RecordOutcome, RecordError> {
    reject_oversized_source("ts_source", migration_source)?;

    let ir_value = record_migration_value_unsandboxed(host, migration_source, owner_app, name)?;
    let ir = ir_from_value(ir_value)?;
    let warnings = advisory_determinism_findings(host, migration_source);
    Ok(RecordOutcome { ir, warnings })
}

/// Map a seam `HostEvalError` back to the historical `RecordError` variant so the
/// observable error is byte-identical (design §2.2): an install/load failure was
/// `RecordError::V8`; a missing/non-string (or failed input-alloc) global was
/// `RecordError::NoIr`.
fn record_error_from_host_eval(e: HostEvalError) -> RecordError {
    match e {
        HostEvalError::Install(msg) => RecordError::V8(msg),
        HostEvalError::MissingOutput(_) => RecordError::NoIr,
    }
}

fn record_migration_value_unsandboxed(
    host: &impl AuthoringHost,
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<serde_json::Value, RecordError> {
    let modules = module_graph(FrontendProgram::RecordMigration {
        source: migration_source,
    });

    // The whole isolate mechanism (init_v8 + Runtime::build + with_scope +
    // load_modules + the raw v8:: global set/get + microtask drain) lives in the
    // adapter's `eval_frontend`. owner_app is DELIBERATELY NOT an input global here:
    // it is a tenant-identifying field folded into the authoritative Checksum::of_ir,
    // so untrusted up() must have no JS-reachable handle to it — the engine stamps
    // owner_app in Rust below (PR4a code-critic HIGH #1; symmetric with the sandboxed
    // recorder child).
    let mut outputs = host
        .eval_frontend(
            &modules,
            GlobalsProfile::Migration,
            &[("__zsMigrationName", name)],
            &["__zsOpIR"],
            None,
        )
        .map_err(record_error_from_host_eval)?;

    let ir_json = outputs.remove("__zsOpIR").ok_or(RecordError::NoIr)?;
    let envelope: OpIrEnvelope =
        serde_json::from_str(&ir_json).map_err(|e| RecordError::Recording(e.to_string()))?;

    if !envelope.ok {
        return Err(RecordError::Recording(
            envelope.error.unwrap_or_else(|| "unknown".into()),
        ));
    }
    let mut ir_value = envelope.ir.ok_or(RecordError::NoIr)?;
    // Rust-stamp the AUTHORITATIVE owner_app onto the recorded IR (HIGH #1): the JS
    // recorder emits ONLY ops, never the tenant-identifying owner. We set it here from
    // the trusted, server-supplied owner_app — overwriting any value untrusted code
    // could have produced. An empty owner leaves the field unset (prior shape).
    if let Some(obj) = ir_value.as_object_mut() {
        if owner_app.is_empty() {
            obj.remove("owner_app");
        } else {
            obj.insert(
                "owner_app".to_string(),
                serde_json::Value::String(owner_app.to_string()),
            );
        }
    }
    Ok(ir_value)
}

fn ir_from_value(ir_value: serde_json::Value) -> Result<MigrationIr, RecordError> {
    // Re-serialize the recorded envelope to canonical JSON bytes, then deserialize
    // through the REAL `MigrationIr` — so the recorded ops pass the SAME frozen
    // wire contract (camelCase op fields, closed Op/Expr AST, the `< 2^53` numeric
    // domain) every deployed `.ir.json` passes. An out-of-contract op fails HERE.
    let bytes = serde_json::to_string(&ir_value)
        .map_err(|e| RecordError::Recording(e.to_string()))?;
    let ir = serde_json::from_str::<MigrationIr>(&bytes)
        .map_err(|e| RecordError::Contract(e.to_string()))?;
    Ok(resolve_create_table_policy(&ir, &PolicyProfile::confined())?)
}

/// **UNSANDBOXED, in-process — for the test oracle / trusted-input use ONLY.**
/// See [`record_migration_to_ir_unsandboxed`]'s safety note (the §8.9
/// in-process-eval threat):
/// the PR4 build/CLI path records via the kernel-sandboxed child, never here.
///
/// Record a migration module and emit its canonical `.ir.json` STRING (the
/// committed-corpus form). Pretty-printed with a trailing newline (POSIX-clean),
/// matching the golden-file convention used by `op-ir.schema.json`.
///
/// # Errors
/// See [`RecordError`].
#[doc(hidden)]
pub fn record_migration_to_json_unsandboxed(
    host: &impl AuthoringHost,
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<String, RecordError> {
    let ir = record_migration_to_ir_unsandboxed(host, migration_source, owner_app, name)?;
    let mut s = serde_json::to_string_pretty(&ir)
        .map_err(|e| RecordError::Recording(e.to_string()))?;
    s.push('\n');
    Ok(s)
}

/// One §4.3 determinism-lint finding (the machine-readable envelope the AI loop
/// self-corrects on). Mirrors the JS `lintDeterminism` finding shape.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeterminismFinding {
    /// The structured error code (`NONDETERMINISTIC_OP_ARG`).
    pub code: String,
    /// The flagged accessor (`Date.now()`, `Math.random()`, …).
    pub accessor: String,
    /// The human-facing steer to the DB-evaluated `c.fn.*` scalar.
    pub suggested_fix: String,
    /// Why the accessor is a non-determinism hazard.
    pub reason: String,
}

fn reject_oversized_source(field: &'static str, source: &str) -> Result<(), RecordError> {
    let actual = source.len();
    if actual > MAX_TS_SOURCE_BYTES {
        Err(RecordError::SourceTooLarge {
            field,
            limit: MAX_TS_SOURCE_BYTES,
            actual,
        })
    } else {
        Ok(())
    }
}

fn advisory_determinism_findings(
    host: &impl AuthoringHost,
    migration_source: &str,
) -> Vec<DeterminismFinding> {
    lint_migration_determinism(host, migration_source).unwrap_or_default()
}

/// Run the §4.3 determinism lint over a migration's SOURCE through the REAL V8
/// `@zeroship/migrate` `lintDeterminism` (NOT a Rust re-implementation) — the
/// faithful path the build/CLI uses to flag a non-deterministic accessor
/// (`Date.now()` / `Math.random()` / `crypto.randomUUID()` / `new Date()`) in an
/// op argument before commit.
///
/// # Errors
/// See [`RecordError`].
pub fn lint_migration_determinism(
    host: &impl AuthoringHost,
    migration_source: &str,
) -> Result<Vec<DeterminismFinding>, RecordError> {
    reject_oversized_source("ts_source", migration_source)?;

    let modules = module_graph(FrontendProgram::DeterminismLint);

    let mut outputs = host
        .eval_frontend(
            &modules,
            GlobalsProfile::Migration,
            &[("__zsLintSrc", migration_source)],
            &["__zsLintOut"],
            None,
        )
        .map_err(record_error_from_host_eval)?;
    let out_json = outputs.remove("__zsLintOut").ok_or(RecordError::NoIr)?;

    #[derive(serde::Deserialize)]
    struct LintEnvelope {
        ok: bool,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        findings: Vec<DeterminismFinding>,
    }

    let env: LintEnvelope = serde_json::from_str(&out_json)
        .map_err(|e| RecordError::Recording(e.to_string()))?;
    if !env.ok {
        return Err(RecordError::Recording(
            env.error.unwrap_or_else(|| "lint failed".into()),
        ));
    }
    Ok(env.findings)
}
