//! V8-backed `op.*` migration recording → `.ir.json` (design §2.5 / PR1 skeletal
//! JS builder).
//!
//! This module evaluates a creator migration module (which calls the
//! `@zeroship/migrate` `op.*` DSL inside `up()`) in zeroship-runtime's V8 sandbox
//! and records the emitted op list into a [`MigrationIr`] — the SAME typed IR the
//! lean engine's `.ir.json` loader deserializes. It is the JS half of the PR1
//! anti-drift gate: a Rust test re-canonicalizes the recorded ops through
//! [`Checksum::of_ir`](crate::Checksum::of_ir) and asserts value
//! equality with the committed golden `.ir.json` fixture (§2.5).
//!
//! Like [`super::eval`] this is one of the places V8 enters the migrate family, and
//! it reuses the runtime's existing sandbox + module loader (no second JS engine).
//! The recorder runs untrusted creator JS under the same sandbox parity as app
//! code (§8.9).

use crate::MigrationIr;
use zeroship_runtime::Runtime;

use super::embedding::{install_frontend_globals, module_graph, FrontendGlobals, FrontendProgram};

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
}

/// A recorded migration plus the §4.3 determinism WARNINGS surfaced on it.
///
/// Per §4.3 the syntactic clock/RNG lint is a **best-effort pre-commit catch**,
/// NOT a hard reject: a non-deterministic accessor (`Date.now()`/`Math.random()`/
/// `crypto.randomUUID()`/`new Date()`) is surfaced as a structured warning the AI
/// loop self-corrects on (§8.8) — recording still succeeds (a build-once committed
/// artifact already neutralizes post-deploy non-determinism, so blanket-rejecting
/// would only raise iteration cost, §4.3). The strict *reject* the design reserves
/// is the narrower type-aware check (a non-literal bound to a time/uuid column),
/// not this whole-source scan. Callers (the build/CLI/recorder service) render the
/// `warnings` alongside the produced `.ir.json`.
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

/// **UNSANDBOXED, in-process — NOT the build path. Hidden from the public API.**
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
/// round-trip test oracle (`op_round_trip`, `full_surface`) and is therefore
/// `#[doc(hidden)]` + crate-`pub(crate)`-equivalent in intent — a library consumer
/// must record through the sandboxed child, never here.
///
/// `owner_app` stamps the recorded `owner_app` HINT (the engine server-stamps the
/// authoritative one at deploy, §8.6). `name` is the filename-derived label used
/// when the module omits an explicit `name`.
///
/// Returns the recorded `MigrationIr` (already validated against the frozen wire
/// contract by `serde` — an out-of-contract op fails [`RecordError::Contract`]).
/// The §4.3 determinism warnings (if any) are DISCARDED on this surface; use
/// [`record_migration_to_ir_with_warnings`] to receive them.
///
/// # Errors
/// See [`RecordError`].
#[doc(hidden)]
pub fn record_migration_to_ir(
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<MigrationIr, RecordError> {
    Ok(record_migration_to_ir_with_warnings(migration_source, owner_app, name)?.ir)
}

/// **UNSANDBOXED, in-process — NOT the build path. Hidden from the public API.**
/// See [`record_migration_to_ir`]'s safety note (the §8.9 in-process-eval threat):
/// the PR4 build/CLI path records via the kernel-sandboxed child, never here.
///
/// Record a migration's `up()` into a typed [`MigrationIr`] AND surface the §4.3
/// determinism warnings on it (the wired pre-commit catch — §4.3/§8.8).
///
/// This is the same record path as [`record_migration_to_ir`], but it runs the REAL
/// V8 `lintDeterminism` over the migration SOURCE and returns its findings as
/// [`RecordOutcome::warnings`] alongside the produced IR. Recording is NOT
/// fail-closed on a finding: per §4.3 the syntactic clock/RNG lint is a best-effort
/// flag the AI loop self-corrects on (the build-once committed artifact already
/// makes post-deploy non-determinism unreachable, §5.1), so a `Date.now()` is
/// surfaced as a warning rather than blocking the artifact. The build/CLI/recorder
/// service render these warnings next to the committed `.ir.json` (§8.8).
///
/// # Errors
/// See [`RecordError`]. A determinism finding is a WARNING, not an error.
#[doc(hidden)]
pub fn record_migration_to_ir_with_warnings(
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<RecordOutcome, RecordError> {
    zeroship_runtime::init_v8();

    // §4.3 pre-commit catch — the WIRED determinism lint. The record/build path runs
    // the REAL V8 `lintDeterminism` over the migration SOURCE and surfaces a
    // non-deterministic accessor (`Date.now()`/`Math.random()`/`crypto.randomUUID()`/
    // `new Date()`) as a structured warning the AI loop self-corrects on (steer to
    // `c.fn.now()`/`c.fn.genRandomUuid()`). It is intentionally NOT fail-closed:
    // §4.3 reserves the hard reject for the narrower type-aware check (a non-literal
    // bound to a time/uuid column), since the build-once committed artifact (§5.1)
    // already neutralizes post-deploy non-determinism and blanket-rejecting would
    // only raise the AI loop's iteration cost.
    let warnings = lint_migration_determinism(migration_source)?;

    let modules = module_graph(FrontendProgram::RecordMigration {
        source: migration_source,
    });

    let runtime = Runtime::builder().build();

    let ir_json: Result<String, RecordError> = runtime.with_scope(|scope| {
        install_frontend_globals(scope, FrontendGlobals::Migration).map_err(RecordError::V8)?;

        // Expose ONLY the filename-derived name to the adapter. owner_app is
        // DELIBERATELY NOT a global: it is a tenant-identifying field folded into the
        // authoritative Checksum::of_ir, so untrusted up() must have no JS-reachable
        // handle to it — the engine stamps owner_app in Rust below (PR4a code-critic
        // HIGH #1; symmetric with the sandboxed recorder child).
        {
            let global = scope.get_current_context().global(scope);
            let k = v8::String::new(scope, "__zsMigrationName").ok_or(RecordError::NoIr)?;
            let v = v8::String::new(scope, name).ok_or(RecordError::NoIr)?;
            global.set(scope, k.into(), v.into());
        }

        zeroship_runtime::modules::load_modules(scope, &modules).map_err(RecordError::V8)?;
        scope.perform_microtask_checkpoint();

        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, "__zsOpIR").unwrap();
        let v = global
            .get(scope, k.into())
            .filter(|v| v.is_string())
            .ok_or(RecordError::NoIr)?;
        Ok(v.to_rust_string_lossy(scope))
    });

    let ir_json = ir_json?;
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
    // Re-serialize the recorded envelope to canonical JSON bytes, then deserialize
    // through the REAL `MigrationIr` — so the recorded ops pass the SAME frozen
    // wire contract (camelCase op fields, closed Op/Expr AST, the `< 2^53` numeric
    // domain) every deployed `.ir.json` passes. An out-of-contract op fails HERE.
    let bytes = serde_json::to_string(&ir_value)
        .map_err(|e| RecordError::Recording(e.to_string()))?;
    let ir = serde_json::from_str::<MigrationIr>(&bytes)
        .map_err(|e| RecordError::Contract(e.to_string()))?;
    Ok(RecordOutcome { ir, warnings })
}

/// **UNSANDBOXED, in-process — NOT the build path. Hidden from the public API.**
/// See [`record_migration_to_ir`]'s safety note (the §8.9 in-process-eval threat):
/// the PR4 build/CLI path records via the kernel-sandboxed child, never here.
///
/// Record a migration module and emit its canonical `.ir.json` STRING (the
/// committed-corpus form). Pretty-printed with a trailing newline (POSIX-clean),
/// matching the golden-file convention used by `op-ir.schema.json`.
///
/// # Errors
/// See [`RecordError`].
#[doc(hidden)]
pub fn record_migration_to_json(
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<String, RecordError> {
    let ir = record_migration_to_ir(migration_source, owner_app, name)?;
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

/// Run the §4.3 determinism lint over a migration's SOURCE through the REAL V8
/// `@zeroship/migrate` `lintDeterminism` (NOT a Rust re-implementation) — the
/// faithful path the build/CLI uses to flag a non-deterministic accessor
/// (`Date.now()` / `Math.random()` / `crypto.randomUUID()` / `new Date()`) in an
/// op argument before commit.
///
/// # Errors
/// See [`RecordError`].
pub fn lint_migration_determinism(
    migration_source: &str,
) -> Result<Vec<DeterminismFinding>, RecordError> {
    zeroship_runtime::init_v8();

    let modules = module_graph(FrontendProgram::DeterminismLint);

    let runtime = Runtime::builder().build();

    let out_json: Result<String, RecordError> = runtime.with_scope(|scope| {
        install_frontend_globals(scope, FrontendGlobals::Migration).map_err(RecordError::V8)?;

        {
            let global = scope.get_current_context().global(scope);
            let k = v8::String::new(scope, "__zsLintSrc").unwrap();
            let v = v8::String::new(scope, migration_source).unwrap();
            global.set(scope, k.into(), v.into());
        }

        zeroship_runtime::modules::load_modules(scope, &modules).map_err(RecordError::V8)?;
        scope.perform_microtask_checkpoint();

        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, "__zsLintOut").unwrap();
        let v = global
            .get(scope, k.into())
            .filter(|v| v.is_string())
            .ok_or(RecordError::NoIr)?;
        Ok(v.to_rust_string_lossy(scope))
    });

    #[derive(serde::Deserialize)]
    struct LintEnvelope {
        ok: bool,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        findings: Vec<DeterminismFinding>,
    }

    let env: LintEnvelope = serde_json::from_str(&out_json?)
        .map_err(|e| RecordError::Recording(e.to_string()))?;
    if !env.ok {
        return Err(RecordError::Recording(
            env.error.unwrap_or_else(|| "lint failed".into()),
        ));
    }
    Ok(env.findings)
}
