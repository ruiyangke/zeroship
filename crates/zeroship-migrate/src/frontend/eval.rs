//! Sandboxed V8-backed `schema.js` evaluation → descriptor IR.
//!
//! This module is the heart of the JS schema front-end (design §5.1). It
//! evaluates an (already-bundled, self-contained) creator schema module in the
//! same kernel-sandboxed recorder child used for op.* recording, then lowers it
//! to the engine's descriptor IR ([`CollectionDescriptor`]).
//!
//! ## Why this is the right reuse (and not a second JS engine)
//!
//! - **Sandbox parity.** The creator `schema.js` is untrusted (AI- or
//!   creator-authored). Evaluating it in the recorder child means it gets the
//!   same resource budget, wall watchdog, and kernel layers as migration
//!   recording: no ambient Node, no fs, no network.
//! - **No cycle.** `zeroship-runtime` depends on neither `zeroship-migrate`
//!   nor `zeroship-schema`; this crate sits ABOVE all three. V8 enters the
//!   migrate family ONLY here — the lean `zeroship-migrate` core stays
//!   V8-free (verified by `cargo tree`).
//! - **Pure lowering.** We do NOT run the app's `installSchema` (which is
//!   welded to a native `env.db`). We eval a tiny adapter
//!   ([`IR_ADAPTER_JS`]) that imports the schema, calls the PURE
//!   `TypeBuilder.toFieldDef()`, and emits the IR JSON — see
//!   `ir_adapter.js`.
//!
//! ## Input contract
//!
//! The input is a **self-contained `schema.js`** — bundled JS where the
//! `@zeroship/db` `t.*` DSL is resolvable (we provide it in the module
//! graph). A raw `schema.ts` (TypeScript source importing npm packages) is
//! NOT a valid input here: TS transpile + npm resolution live in the JS
//! build pipeline (esbuild/Vite), not in the Rust runtime — the runtime's
//! module loader compiles raw source straight to a V8 module and has no
//! transpile step. See the crate README / the P3 report for the gap.

use crate::render::declarative::CollectionDescriptor;

use super::recorder_service::{spawn_sandboxed_schema_eval, RecorderError, SchemaEvalRequest};
use super::sandbox::{ResourceBudget, SandboxPosture};

/// An error from evaluating a `schema.js`.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// V8 module loading / evaluation failed (syntax error, throw at module
    /// top level, unresolved import, …). Carries the runtime's error string.
    #[error("schema.js evaluation failed: {0}")]
    V8(String),
    /// The sandboxed child could not complete evaluation because the resource
    /// budget/kernel containment fired or the child infrastructure failed.
    #[error("schema sandbox failed: {0}")]
    Sandbox(String),
    /// The adapter ran but reported a lowering error (e.g. the module does not
    /// expose a schema authoring map, or a field is not a `t.*` builder).
    #[error("schema lowering failed: {0}")]
    Lowering(String),
    /// The adapter did not leave the expected `globalThis.__zsSchemaIR`
    /// string (should not happen — indicates a glue/runtime contract break).
    #[error("schema front-end produced no IR (internal contract break)")]
    NoIr,
    /// The IR JSON could not be deserialized into [`CollectionDescriptor`]s.
    #[error("descriptor IR deserialization failed: {0}")]
    Deserialize(String),
}

/// The deserialized adapter result mirroring the JSON the glue emits on
/// `globalThis.__zsSchemaIR`.
#[derive(serde::Deserialize)]
struct IrEnvelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    collections: Vec<CollectionDescriptor>,
}

/// Evaluate a self-contained `schema.js` source in the V8 sandbox and lower
/// it to the descriptor IR.
///
/// `owner_app` stamps the declaring app on every emitted
/// [`CollectionDescriptor::owner_app`] (the project-umbrella ownership
/// subject). `schema_source` is the bundled JS — it may
/// `import { t, schema } from "@zeroship/db"` (resolved to the embedded
/// DSL) and expose a schema map for the authoring front-end to lower.
///
/// # Errors
/// See [`EvalError`].
pub fn eval_schema_to_ir(
    schema_source: &str,
    owner_app: &str,
) -> Result<Vec<CollectionDescriptor>, EvalError> {
    eval_schema_to_ir_with_budget(schema_source, owner_app, ResourceBudget::default())
}

/// Evaluate schema JS with an explicit child budget. Test-only callers use this
/// to keep budget-containment regressions fast; production uses
/// [`eval_schema_to_ir`].
#[doc(hidden)]
pub fn eval_schema_to_ir_with_budget(
    schema_source: &str,
    owner_app: &str,
    budget: ResourceBudget,
) -> Result<Vec<CollectionDescriptor>, EvalError> {
    let req = SchemaEvalRequest {
        schema_source: schema_source.to_string(),
        owner_app: owner_app.to_string(),
        posture: SandboxPosture::Local,
        budget,
        allow_read_paths: vec![],
    };
    let ir_json = match spawn_sandboxed_schema_eval(&req) {
        Ok(result) => result.schema_ir_json,
        Err(RecorderError::EvalError(message)) => return Err(EvalError::V8(message)),
        Err(err) => return Err(EvalError::Sandbox(format!("{}: {err}", err.code()))),
    };

    let envelope: IrEnvelope = serde_json::from_str(&ir_json)
        .map_err(|e| EvalError::Deserialize(e.to_string()))?;

    if !envelope.ok {
        return Err(EvalError::Lowering(
            envelope.error.unwrap_or_else(|| "unknown".into()),
        ));
    }
    let mut collections = envelope.collections;
    for collection in &mut collections {
        collection.owner_app = owner_app.to_string();
    }
    Ok(collections)
}
