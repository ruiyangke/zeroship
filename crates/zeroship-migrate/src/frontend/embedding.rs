//! Shared V8 embedding for the migrate JS front-end.
//!
//! Schema eval, the in-process recorder oracle, the determinism lint, and the
//! sandboxed recorder child all load the same trusted adapter/DSL modules from
//! this file. The entry module changes per operation, but the module universe
//! and globals are owned here so subpath shims and host globals cannot drift.

use zeroship_runtime::ModuleEntry;

/// The op.* recorder adapter glue.
const OP_RECORDER_JS: &str = include_str!("op_recorder.js");

/// The schema IR lowering adapter glue.
const IR_ADAPTER_JS: &str = include_str!("ir_adapter.js");

/// The bundled `@zeroship/migrate` op.* DSL.
///
/// The ONE compiled recorder artifact (DSL redesign S0.5): the SDK recorder and
/// this engine-embedded recorder are the same `tsup` build output of
/// `sdks/migrate/src/{ops,pg}.ts` (via the `src/embedded-recorder.ts` entry).
/// The former hand-kept `migrate_ops.js` twin is deleted; there is no byte-parity
/// to maintain, only build provenance. `pnpm build` MUST run before
/// `cargo build -p zeroship-migrate` (root `pnpm build` emits this dist file in
/// dependency order) — the same ordering this file already relies on for the
/// `@zeroship/db` bundle below.
const MIGRATE_OPS_JS: &str = include_str!("../../../../sdks/migrate/dist/embedded-recorder.js");

/// The bundled `@zeroship/db` schema DSL.
const ZEROSHIP_DB_DIST_JS: &str = include_str!("../../../../sdks/db/dist/index.js");

/// The vendored `mysql2/promise` bundle for the Trusted JS-driver isolate.
const MYSQL2_PROMISE_BUNDLE_JS: &str = include_str!("vendor/mysql2-3.14.1.bundle.mjs");

/// `@zeroship/migrate/pg` is a subpath shim over the same recorder-state module.
const MIGRATE_PG_SHIM_JS: &str = r#"
export {
  schema,
  dropSchema,
  extension,
  dropExtension,
  role,
  alterRole,
  dropRole,
  dropOwnedBy,
  grant,
  revoke,
  pgTable,
  createPolicy,
  dropPolicy,
  createFunction,
  dropFunction,
  raw,
  __pgDomain as domain,
  __pgSequence as sequence,
} from "@zeroship/migrate";
"#;

/// Minimal `zeroship` facade required by the `@zeroship/db` bundle.
const ZEROSHIP_FACADE_STUB_JS: &str = r#"
export const env = Object.freeze({});
export function waitUntil() {}
export function getRequest() { return null; }
export function getRequestContext() { return undefined; }
export default {};
"#;

/// The determinism-lint entry module.
const DETERMINISM_LINT_JS: &str = r#"
import { lintDeterminism } from "@zeroship/migrate";
try {
  const findings = lintDeterminism(globalThis.__zsLintSrc || "");
  globalThis.__zsLintOut = JSON.stringify({ ok: true, findings });
} catch (e) {
  globalThis.__zsLintOut = JSON.stringify({ ok: false, error: (e && e.message) ? e.message : String(e) });
}
export default {};
"#;

const EMPTY_MIGRATION_JS: &str = "export function up() {}\n";
const EMPTY_SCHEMA_JS: &str = "export default { schema: {} };\n";

/// Which front-end program is the graph entrypoint.
#[derive(Debug, Clone, Copy)]
pub enum FrontendProgram<'a> {
    RecordMigration { source: &'a str },
    EvalSchema { source: &'a str },
    DeterminismLint,
}

/// The global installer profile a front-end operation needs.
#[derive(Debug, Clone, Copy)]
pub enum FrontendGlobals {
    Migration,
    Schema,
}

fn module(specifier: &str, source: &str) -> ModuleEntry {
    ModuleEntry {
        specifier: specifier.to_string(),
        source: source.to_string(),
    }
}

/// Build the canonical in-memory module graph for a migrate front-end operation.
///
/// Only the first entry is eagerly compiled by the runtime loader; the remaining
/// entries form the shared source map. Keeping both adapters and both user-entry
/// specifiers in this graph makes the import allow-list identical across eval,
/// record, lint, and the sandboxed child.
pub fn module_graph(program: FrontendProgram<'_>) -> Vec<ModuleEntry> {
    let mut modules = Vec::with_capacity(10);

    match program {
        FrontendProgram::RecordMigration { source } => {
            modules.push(module("op_recorder.js", OP_RECORDER_JS));
            modules.push(module("__migration__.js", source));
            modules.push(module("ir_adapter.js", IR_ADAPTER_JS));
            modules.push(module("__schema__.js", EMPTY_SCHEMA_JS));
        }
        FrontendProgram::EvalSchema { source } => {
            modules.push(module("ir_adapter.js", IR_ADAPTER_JS));
            modules.push(module("__schema__.js", source));
            modules.push(module("op_recorder.js", OP_RECORDER_JS));
            modules.push(module("__migration__.js", EMPTY_MIGRATION_JS));
        }
        FrontendProgram::DeterminismLint => {
            modules.push(module("determinism_lint.js", DETERMINISM_LINT_JS));
            modules.push(module("op_recorder.js", OP_RECORDER_JS));
            modules.push(module("__migration__.js", EMPTY_MIGRATION_JS));
            modules.push(module("ir_adapter.js", IR_ADAPTER_JS));
            modules.push(module("__schema__.js", EMPTY_SCHEMA_JS));
        }
    }

    modules.push(module("@zeroship/migrate", MIGRATE_OPS_JS));
    modules.push(module("@zeroship/migrate/pg", MIGRATE_PG_SHIM_JS));
    modules.push(module("@zeroship/db", ZEROSHIP_DB_DIST_JS));
    modules.push(module("zeroship", ZEROSHIP_FACADE_STUB_JS));

    modules
}

/// Build the module graph for the platform-owned Trusted JS-driver isolate.
///
/// `entry_source` is the long-lived command-loop driver entry. The mysql2
/// bundle is registered under the package specifier the entry imports:
/// `import mysql from "mysql2/promise"`.
pub(crate) fn js_driver_module_graph(entry_source: &str) -> Vec<ModuleEntry> {
    vec![
        module("js_driver_entry.js", entry_source),
        module("mysql2/promise", MYSQL2_PROMISE_BUNDLE_JS),
    ]
}

/// Install the canonical globals for a migrate front-end operation.
pub fn install_frontend_globals(
    scope: &mut v8::PinScope,
    globals: FrontendGlobals,
) -> Result<(), String> {
    zeroship_runtime::init::setup_globals(scope)?;
    match globals {
        FrontendGlobals::Migration => {
            zeroship_runtime::init::install_text_encoding_streams(scope);
        }
        FrontendGlobals::Schema => {
            zeroship_runtime::init::install_headers(scope);
            zeroship_runtime::init::install_native_streams(scope);
            zeroship_runtime::init::install_blob_native(scope);
            zeroship_runtime::init::install_text_encoding_streams(scope);
            zeroship_runtime::init::install_dom(scope);
        }
    }
    Ok(())
}
