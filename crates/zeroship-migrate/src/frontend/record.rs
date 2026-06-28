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

use std::collections::BTreeSet;

use crate::model::ir::CanonicalOpList;
use crate::{Checksum, MigrationFlags, MigrationIr};
use zeroship_runtime::Runtime;

use super::embedding::{
    install_frontend_globals, module_graph, read_determinism_probe_used, DeterminismProbeSeed,
    FrontendGlobals, FrontendProgram,
};
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
    /// The determinism probe observed host-clock/RNG invocation, or the secondary
    /// record-twice check found host-clock/RNG-dependent IR output.
    #[error(
        "nondeterministic authoring JS uses {accessors} at {differing_path}; use DB-evaluated c.fn.* scalars instead"
    )]
    Nondeterministic {
        /// Comma-separated accessor summary when the advisory source lint can name it.
        accessors: String,
        /// First differing IR path/field.
        differing_path: String,
        /// Advisory structured lint findings. These are hints only; the hard error
        /// comes from probe-observed invocation or recorded IR divergence.
        findings: Vec<DeterminismFinding>,
    },
}

/// A recorded migration plus the determinism lint outcome.
///
/// Detected nondeterminism is now a hard error. This wrapper keeps the older test
/// oracle shape; successful recordings may carry advisory source-lint warnings.
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

struct RecordProbe {
    ir_value: serde_json::Value,
    nondeterminism_used: Vec<String>,
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
/// Detected nondeterminism is a hard [`RecordError::Nondeterministic`].
///
/// # Errors
/// See [`RecordError`].
#[doc(hidden)]
pub fn record_migration_to_ir_unsandboxed(
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<MigrationIr, RecordError> {
    Ok(record_migration_to_ir_with_warnings_unsandboxed(migration_source, owner_app, name)?.ir)
}

/// **UNSANDBOXED, in-process — for the test oracle / trusted-input use ONLY.**
/// See [`record_migration_to_ir_unsandboxed`]'s safety note (the §8.9
/// in-process-eval threat):
/// the PR4 build/CLI path records via the kernel-sandboxed child, never here.
///
/// Record a migration's `up()` into a typed [`MigrationIr`] after the determinism
/// probe proves the source did not invoke known host-clock/RNG APIs and the
/// record-twice secondary check proves the emitted IR is stable.
///
/// This is the same record path as [`record_migration_to_ir_unsandboxed`], but it
/// keeps the historical [`RecordOutcome`] wrapper used by tests.
/// If the probe observes invocation of `Date.now()`/`Date()`/argless
/// `new Date()`/`performance.now()`/`Math.random()`/`crypto.getRandomValues()`/
/// `crypto.randomUUID()`, this returns hard [`RecordError::Nondeterministic`].
/// The record-twice IR/checksum comparison remains as a secondary defense for
/// residual unknown nondeterminism. The legacy regex lint is retained only as an
/// advisory warning source and never gates recording.
///
/// # Errors
/// See [`RecordError`].
#[doc(hidden)]
pub fn record_migration_to_ir_with_warnings_unsandboxed(
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<RecordOutcome, RecordError> {
    reject_oversized_source("ts_source", migration_source)?;

    let probe_a = record_migration_value_unsandboxed(
        migration_source,
        owner_app,
        name,
        Some(DeterminismProbeSeed::A),
    )?;
    let probe_b = record_migration_value_unsandboxed(
        migration_source,
        owner_app,
        name,
        Some(DeterminismProbeSeed::B),
    )?;

    let nondeterminism_used =
        merge_nondeterminism_used(&probe_a.nondeterminism_used, &probe_b.nondeterminism_used);
    if !nondeterminism_used.is_empty() {
        let findings = advisory_determinism_findings(migration_source);
        return Err(RecordError::Nondeterministic {
            accessors: nondeterminism_used_summary(&nondeterminism_used),
            differing_path: "$.nondeterminismUsed".to_string(),
            findings,
        });
    }

    if probe_a.ir_value != probe_b.ir_value {
        let differing_path =
            first_json_difference_path(&probe_a.ir_value, &probe_b.ir_value)
                .unwrap_or_else(|| "$".to_string());
        let findings = advisory_determinism_findings(migration_source);
        return Err(RecordError::Nondeterministic {
            accessors: accessor_summary(&findings),
            differing_path,
            findings,
        });
    }

    let ir_a = ir_from_value(probe_a.ir_value)?;
    let ir_b = ir_from_value(probe_b.ir_value)?;
    let checksum_a = determinism_checksum(&ir_a);
    let checksum_b = determinism_checksum(&ir_b);
    if checksum_a != checksum_b {
        let findings = advisory_determinism_findings(migration_source);
        return Err(RecordError::Nondeterministic {
            accessors: accessor_summary(&findings),
            differing_path: "$.checksum".to_string(),
            findings,
        });
    }
    let warnings = advisory_determinism_findings(migration_source);
    Ok(RecordOutcome {
        ir: ir_a,
        warnings,
    })
}

fn record_migration_value_unsandboxed(
    migration_source: &str,
    owner_app: &str,
    name: &str,
    determinism_probe_seed: Option<DeterminismProbeSeed>,
) -> Result<RecordProbe, RecordError> {
    zeroship_runtime::init_v8();

    let modules = module_graph(FrontendProgram::RecordMigration {
        source: migration_source,
    });

    let runtime = Runtime::builder().build();

    let probe: Result<(String, Vec<String>), RecordError> = runtime.with_scope(|scope| {
        install_frontend_globals(
            scope,
            FrontendGlobals::Migration,
            determinism_probe_seed,
        )
        .map_err(RecordError::V8)?;

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
        let nondeterminism_used =
            read_determinism_probe_used(scope).map_err(RecordError::V8)?;
        Ok((v.to_rust_string_lossy(scope), nondeterminism_used))
    });

    let (ir_json, nondeterminism_used) = probe?;
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
    Ok(RecordProbe {
        ir_value,
        nondeterminism_used,
    })
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
    Ok(ir)
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
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<String, RecordError> {
    let ir = record_migration_to_ir_unsandboxed(migration_source, owner_app, name)?;
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

fn advisory_determinism_findings(migration_source: &str) -> Vec<DeterminismFinding> {
    lint_migration_determinism(migration_source).unwrap_or_default()
}

pub(crate) fn accessor_summary(findings: &[DeterminismFinding]) -> String {
    if findings.is_empty() {
        return "Date.now()/Date()/new Date()/performance.now()/Math.random()/crypto randomness"
            .to_string();
    }
    let accessors = findings
        .iter()
        .map(|f| f.accessor.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    accessors
}

pub(crate) fn merge_nondeterminism_used(a: &[String], b: &[String]) -> Vec<String> {
    a.iter()
        .chain(b.iter())
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(str::to_string)
        .collect()
}

pub(crate) fn nondeterminism_used_summary(used: &[String]) -> String {
    if used.is_empty() {
        return accessor_summary(&[]);
    }
    used.iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn determinism_checksum(ir: &MigrationIr) -> String {
    Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    )
    .as_str()
    .to_string()
}

pub(crate) fn first_json_difference_path(
    a: &serde_json::Value,
    b: &serde_json::Value,
) -> Option<String> {
    fn fmt(path: &[String]) -> String {
        if path.is_empty() {
            "$".to_string()
        } else {
            format!("${}", path.join(""))
        }
    }

    fn walk(
        a: &serde_json::Value,
        b: &serde_json::Value,
        path: &mut Vec<String>,
    ) -> Option<String> {
        match (a, b) {
            (serde_json::Value::Object(ma), serde_json::Value::Object(mb)) => {
                let keys = ma.keys().chain(mb.keys()).collect::<BTreeSet<_>>();
                for key in keys {
                    path.push(format!(".{key}"));
                    match (ma.get(key), mb.get(key)) {
                        (Some(va), Some(vb)) => {
                            if let Some(found) = walk(va, vb, path) {
                                return Some(found);
                            }
                        }
                        _ => return Some(fmt(path)),
                    }
                    path.pop();
                }
                None
            }
            (serde_json::Value::Array(aa), serde_json::Value::Array(ab)) => {
                let len = aa.len().max(ab.len());
                for i in 0..len {
                    path.push(format!("[{i}]"));
                    match (aa.get(i), ab.get(i)) {
                        (Some(va), Some(vb)) => {
                            if let Some(found) = walk(va, vb, path) {
                                return Some(found);
                            }
                        }
                        _ => return Some(fmt(path)),
                    }
                    path.pop();
                }
                None
            }
            _ if a == b => None,
            _ => Some(fmt(path)),
        }
    }

    walk(a, b, &mut Vec::new())
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
    reject_oversized_source("ts_source", migration_source)?;
    zeroship_runtime::init_v8();

    let modules = module_graph(FrontendProgram::DeterminismLint);

    let runtime = Runtime::builder().build();

    let out_json: Result<String, RecordError> = runtime.with_scope(|scope| {
        install_frontend_globals(scope, FrontendGlobals::Migration, None).map_err(RecordError::V8)?;

        {
            let global = scope.get_current_context().global(scope);
            let k = v8::String::new(scope, "__zsLintSrc").ok_or(RecordError::NoIr)?;
            let v = v8::String::new(scope, migration_source).ok_or(RecordError::NoIr)?;
            global.set(scope, k.into(), v.into());
        }

        zeroship_runtime::modules::load_modules(scope, &modules).map_err(RecordError::V8)?;
        scope.perform_microtask_checkpoint();

        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, "__zsLintOut").ok_or(RecordError::NoIr)?;
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
