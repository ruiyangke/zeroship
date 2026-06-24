//! V8-backed `op.*` migration recording → `.ir.json` (design §2.5 / PR1 skeletal
//! JS builder).
//!
//! This module evaluates a creator migration module (which calls the
//! `@zeroship/migrate` `op.*` DSL inside `up()`) in zeroship-runtime's V8 sandbox
//! and records the emitted op list into a [`MigrationIr`] — the SAME typed IR the
//! lean engine's `.ir.json` loader deserializes. It is the JS half of the PR1
//! anti-drift gate: a Rust test re-canonicalizes the recorded ops through
//! [`Checksum::of_ir`](zeroship_migrate::Checksum::of_ir) and asserts value
//! equality with the committed golden `.ir.json` fixture (§2.5).
//!
//! Like [`crate::eval`] this is the ONLY place V8 enters the migrate family, and
//! it reuses the runtime's existing sandbox + module loader (no second JS engine).
//! The recorder runs untrusted creator JS under the same sandbox parity as app
//! code (§8.9).

use zeroship_migrate::MigrationIr;
use zeroship_runtime::{ModuleEntry, Runtime};

/// The op.* recorder adapter glue (the entry module). Imports the creator
/// migration under `./__migration__.js` + the `@zeroship/migrate` DSL, invokes
/// `up()`, drains the recorded ops, and stashes the `.ir.json` envelope on
/// `globalThis.__zsOpIR`.
const OP_RECORDER_JS: &str = include_str!("op_recorder.js");

/// The minimal `@zeroship/migrate` op.* DSL — the named-import op-functions
/// (`createTable`, `addColumn`, …) + the recording buffer + the `e.*` Expr-node
/// helpers. The migration module's `import { … } from "@zeroship/migrate"`
/// resolves to this.
const MIGRATE_OPS_JS: &str = include_str!("migrate_ops.js");

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

/// Record a self-contained migration module's `up()` op list into a typed
/// [`MigrationIr`] (§2.5 — the JS half of the anti-drift gate).
///
/// `migration_source` is the bundled JS — it may
/// `import { createTable, addColumn, … } from "@zeroship/migrate"` (resolved to
/// the embedded DSL) and `export function up()` (or `export default { up }`).
/// `owner_app` stamps the recorded `owner_app` HINT (the engine server-stamps the
/// authoritative one at deploy, §8.6). `name` is the filename-derived label used
/// when the module omits an explicit `name`.
///
/// Returns the recorded `MigrationIr` (already validated against the frozen wire
/// contract by `serde` — an out-of-contract op fails [`RecordError::Contract`]).
///
/// # Errors
/// See [`RecordError`].
pub fn record_migration_to_ir(
    migration_source: &str,
    owner_app: &str,
    name: &str,
) -> Result<MigrationIr, RecordError> {
    zeroship_runtime::init_v8();

    let modules = vec![
        ModuleEntry {
            specifier: "op_recorder.js".into(),
            source: OP_RECORDER_JS.to_string(),
        },
        ModuleEntry {
            specifier: "__migration__.js".into(),
            source: migration_source.to_string(),
        },
        ModuleEntry {
            specifier: "@zeroship/migrate".into(),
            source: MIGRATE_OPS_JS.to_string(),
        },
    ];

    let runtime = Runtime::builder().build();

    let ir_json: Result<String, RecordError> = runtime.with_scope(|scope| {
        zeroship_runtime::init::setup_globals(scope);
        zeroship_runtime::init::install_text_encoding_streams(scope);

        // Stamp the owner-app hint + the filename-derived name on the globals the
        // adapter reads.
        {
            let global = scope.get_current_context().global(scope);
            for (key, val) in [("__zsOwnerApp", owner_app), ("__zsMigrationName", name)] {
                let k = v8::String::new(scope, key).unwrap();
                let v = v8::String::new(scope, val).unwrap();
                global.set(scope, k.into(), v.into());
            }
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
    let ir_value = envelope.ir.ok_or(RecordError::NoIr)?;
    // Re-serialize the recorded envelope to canonical JSON bytes, then deserialize
    // through the REAL `MigrationIr` — so the recorded ops pass the SAME frozen
    // wire contract (camelCase op fields, closed Op/Expr AST, the `< 2^53` numeric
    // domain) every deployed `.ir.json` passes. An out-of-contract op fails HERE.
    let bytes = serde_json::to_string(&ir_value)
        .map_err(|e| RecordError::Recording(e.to_string()))?;
    serde_json::from_str::<MigrationIr>(&bytes).map_err(|e| RecordError::Contract(e.to_string()))
}

/// Record a migration module and emit its canonical `.ir.json` STRING (the
/// committed-corpus form). Pretty-printed with a trailing newline (POSIX-clean),
/// matching the golden-file convention used by `op-ir.schema.json`.
///
/// # Errors
/// See [`RecordError`].
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
