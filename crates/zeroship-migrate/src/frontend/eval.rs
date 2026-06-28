//! V8-backed `schema.js` evaluation → descriptor IR.
//!
//! This module is the heart of the JS schema front-end (design §5.1). It
//! reuses **zeroship-runtime's existing V8 sandbox** — the SAME isolate +
//! WinterCG polyfill set + ES-module loader that runs untrusted creator app
//! code — to evaluate an (already-bundled, self-contained) creator schema
//! module and lower it to the engine's descriptor IR
//! ([`CollectionDescriptor`]).
//!
//! ## Why this is the right reuse (and not a second JS engine)
//!
//! - **Sandbox parity.** The creator `schema.js` is untrusted (AI- or
//!   creator-authored). Evaluating it in the runtime's isolate means it runs
//!   under the SAME security model as app code: no ambient Node, no fs, no
//!   process — only the WinterCG globals the runtime installs.
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
use zeroship_runtime::{ModuleEntry, Runtime};

/// The IR adapter glue (the entry module of the eval graph). Imports the
/// creator schema under `./__schema__.js` and the `@zeroship/db` DSL, lowers
/// the schema to the engine `CollectionDescriptor` JSON, and stashes it on
/// `globalThis.__zsSchemaIR`.
const IR_ADAPTER_JS: &str = include_str!("ir_adapter.js");

/// The `@zeroship/db` DSL module — the `t.*` builder, `TypeBuilder`,
/// `schema`, etc. Embedded from the package's built `dist/index.js` so the
/// front-end speaks the EXACT same DSL the runtime + creator code do (no
/// re-implementation, no drift). The schema module's
/// `import { t } from "@zeroship/db"` resolves to this.
const ZEROSHIP_DB_DIST_JS: &str =
    include_str!("../../../../sdks/db/dist/index.js");

/// Minimal `zeroship` facade stub. `@zeroship/db`'s dist does a bare
/// side-effect `import 'zeroship';` (no named bindings used at module-eval
/// time), so a stub that simply resolves the specifier is sufficient for
/// schema lowering. We deliberately do NOT pull in the runtime's real
/// `ZEROSHIP_MODULE_JS` (which calls `__zs_env()` at top level and needs the
/// full app-boot global wiring) — the schema front-end is a non-dispatch,
/// CLI-time eval.
const ZEROSHIP_FACADE_STUB_JS: &str = r#"
export const env = Object.freeze({});
export function waitUntil() {}
export function getRequest() { return null; }
export function getRequestContext() { return undefined; }
export default {};
"#;

/// An error from evaluating a `schema.js`.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// V8 module loading / evaluation failed (syntax error, throw at module
    /// top level, unresolved import, …). Carries the runtime's error string.
    #[error("schema.js evaluation failed: {0}")]
    V8(String),
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
    zeroship_runtime::init_v8();

    // The eval module graph. Entry[0] is the adapter glue; it imports the
    // creator schema (`./__schema__.js`) and `@zeroship/db`.
    let modules = vec![
        ModuleEntry {
            specifier: "ir_adapter.js".into(),
            source: IR_ADAPTER_JS.to_string(),
        },
        ModuleEntry {
            specifier: "__schema__.js".into(),
            source: schema_source.to_string(),
        },
        ModuleEntry {
            specifier: "@zeroship/db".into(),
            source: ZEROSHIP_DB_DIST_JS.to_string(),
        },
        ModuleEntry {
            specifier: "zeroship".into(),
            source: ZEROSHIP_FACADE_STUB_JS.to_string(),
        },
    ];

    // A bare Runtime gives us the isolate + context + WinterCG polyfills via
    // `setup_globals` / `install_*`. No plugins, no app_id: this is a pure
    // CLI-time eval, not an app boot.
    let runtime = Runtime::builder().build();

    let ir_json: Result<String, EvalError> = runtime.with_scope(|scope| {
        // Install the WinterCG globals the DSL module's dist may touch at
        // eval time (TextEncoder, structuredClone, URL, …). These are the
        // SAME installers the runtime uses for app boot — sandbox parity.
        zeroship_runtime::init::setup_globals(scope).map_err(EvalError::V8)?;
        zeroship_runtime::init::install_headers(scope);
        zeroship_runtime::init::install_native_streams(scope);
        zeroship_runtime::init::install_blob_native(scope);
        zeroship_runtime::init::install_text_encoding_streams(scope);
        zeroship_runtime::init::install_dom(scope);

        // Stamp the owner app on a global the adapter reads.
        {
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "__zsOwnerApp").unwrap();
            let val = v8::String::new(scope, owner_app).unwrap();
            global.set(scope, key.into(), val.into());
        }

        // Load + evaluate the module graph. The adapter's top-level code
        // (synchronous — no top-level await) runs during this call, so by
        // the time it returns `globalThis.__zsSchemaIR` is set.
        zeroship_runtime::modules::load_modules(scope, &modules)
            .map_err(EvalError::V8)?;

        // Drain microtasks (defensive — the adapter is sync, but the DSL
        // dist eval may schedule some).
        scope.perform_microtask_checkpoint();

        // Read `globalThis.__zsSchemaIR` back out.
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__zsSchemaIR").unwrap();
        let val = global
            .get(scope, key.into())
            .filter(|v| v.is_string())
            .ok_or(EvalError::NoIr)?;
        Ok(val.to_rust_string_lossy(scope))
    });

    let ir_json = ir_json?;
    let envelope: IrEnvelope = serde_json::from_str(&ir_json)
        .map_err(|e| EvalError::Deserialize(e.to_string()))?;

    if !envelope.ok {
        return Err(EvalError::Lowering(
            envelope.error.unwrap_or_else(|| "unknown".into()),
        ));
    }
    Ok(envelope.collections)
}
