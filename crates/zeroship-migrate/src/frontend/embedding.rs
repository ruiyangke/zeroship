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
const MIGRATE_OPS_JS: &str = include_str!("migrate_ops.js");

/// The bundled `@zeroship/db` schema DSL.
const ZEROSHIP_DB_DIST_JS: &str = include_str!("../../../../sdks/db/dist/index.js");

/// `@zeroship/migrate/pg` is a subpath shim over the same recorder-state module.
const MIGRATE_PG_SHIM_JS: &str = r#"export { pg } from "@zeroship/migrate";"#;

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

/// Seed for the build-time determinism probe.
///
/// A seeded recorder run replaces known authoring-time nondeterministic globals
/// with deterministic sequences. Probe A and probe B deliberately use distinct
/// bases, so any recorded IR field that depends on wall-clock/RNG input diverges
/// between the two runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeterminismProbeSeed {
    /// Numeric seed id. `1` and `2` are the canonical record-twice probe pair.
    pub run: u8,
}

impl DeterminismProbeSeed {
    /// First deterministic probe run.
    pub const A: Self = Self { run: 1 };
    /// Second deterministic probe run.
    pub const B: Self = Self { run: 2 };

    fn date_base_ms(self) -> u64 {
        match self.run {
            1 => 1_000_000,
            2 => 2_000_000,
            n => 1_000_000 + (u64::from(n) * 1_000_000),
        }
    }

    fn perf_base_ms(self) -> u64 {
        match self.run {
            1 => 10_000,
            2 => 20_000,
            n => 10_000 + (u64::from(n) * 10_000),
        }
    }

    fn random_base_byte(self) -> u8 {
        match self.run {
            1 => 0x11,
            2 => 0x88,
            n => n.wrapping_mul(73).wrapping_add(17),
        }
    }

    fn random_fraction(self) -> &'static str {
        match self.run {
            1 => "0.1111111111111111",
            2 => "0.8888888888888888",
            _ => "0.4242424242424242",
        }
    }
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

/// Install the canonical globals for a migrate front-end operation.
pub fn install_frontend_globals(
    scope: &mut v8::PinScope,
    globals: FrontendGlobals,
    determinism_probe_seed: Option<DeterminismProbeSeed>,
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
    if let Some(seed) = determinism_probe_seed {
        install_determinism_probe_globals(scope, seed)?;
    }
    Ok(())
}

fn install_determinism_probe_globals(
    scope: &mut v8::PinScope,
    seed: DeterminismProbeSeed,
) -> Result<(), String> {
    let script = format!(
        r#"
(function() {{
  const g = globalThis;
  const OriginalDate = g.Date;
  let dateCounter = 0;
  let perfCounter = 0;
  let byteCounter = 0;
  const dateBase = {date_base};
  const perfBase = {perf_base};
  const randomBase = {random_base};
  const randomFraction = {random_fraction};

  function nextDateMs() {{
    return dateBase + dateCounter++;
  }}

  function ProbeDate(...args) {{
    if (new.target) {{
      if (args.length === 0) {{
        return Reflect.construct(OriginalDate, [nextDateMs()], new.target);
      }}
      return Reflect.construct(OriginalDate, args, new.target);
    }}
    return new OriginalDate(nextDateMs()).toString();
  }}
  Object.setPrototypeOf(ProbeDate, OriginalDate);
  ProbeDate.prototype = OriginalDate.prototype;
  Object.defineProperty(ProbeDate, "now", {{
    value: () => nextDateMs(),
    configurable: true,
    writable: true
  }});
  Object.defineProperty(ProbeDate, "parse", {{
    value: OriginalDate.parse.bind(OriginalDate),
    configurable: true,
    writable: true
  }});
  Object.defineProperty(ProbeDate, "UTC", {{
    value: OriginalDate.UTC.bind(OriginalDate),
    configurable: true,
    writable: true
  }});
  g.Date = ProbeDate;

  if (!g.performance || typeof g.performance !== "object") {{
    g.performance = {{}};
  }}
  Object.defineProperty(g.performance, "now", {{
    value: () => perfBase + perfCounter++,
    configurable: true,
    writable: true
  }});

  Object.defineProperty(g.Math, "random", {{
    value: () => randomFraction,
    configurable: true,
    writable: true
  }});

  function nextByte() {{
    const b = (randomBase + (byteCounter * 17)) & 255;
    byteCounter++;
    return b;
  }}

  function getRandomValues(view) {{
    if (!view || typeof view.length !== "number") {{
      throw new TypeError("getRandomValues requires a typed array");
    }}
    const isBig = typeof BigInt64Array !== "undefined" &&
      (view instanceof BigInt64Array || view instanceof BigUint64Array);
    for (let i = 0; i < view.length; i++) {{
      const byte = nextByte();
      view[i] = isBig ? BigInt(byte) : byte;
    }}
    return view;
  }}

  function randomUUID() {{
    const b = new Uint8Array(16);
    getRandomValues(b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    const hex = Array.from(b, x => x.toString(16).padStart(2, "0"));
    return `${{hex[0]}}${{hex[1]}}${{hex[2]}}${{hex[3]}}-${{hex[4]}}${{hex[5]}}-${{hex[6]}}${{hex[7]}}-${{hex[8]}}${{hex[9]}}-${{hex[10]}}${{hex[11]}}${{hex[12]}}${{hex[13]}}${{hex[14]}}${{hex[15]}}`;
  }}

  const originalCrypto = g.crypto;
  const probeCrypto = Object.create(originalCrypto || null);
  Object.defineProperty(probeCrypto, "getRandomValues", {{
    value: getRandomValues,
    configurable: true,
    writable: true
  }});
  Object.defineProperty(probeCrypto, "randomUUID", {{
    value: randomUUID,
    configurable: true,
    writable: true
  }});
  Object.defineProperty(g, "crypto", {{
    value: probeCrypto,
    configurable: true,
    writable: true
  }});
}})();
"#,
        date_base = seed.date_base_ms(),
        perf_base = seed.perf_base_ms(),
        random_base = seed.random_base_byte(),
        random_fraction = seed.random_fraction(),
    );
    let source = v8::String::new(scope, &script)
        .ok_or_else(|| "determinism probe globals: source alloc failed".to_string())?;
    let compiled = v8::Script::compile(scope, source, None)
        .ok_or_else(|| "determinism probe globals: compile failed".to_string())?;
    compiled
        .run(scope)
        .ok_or_else(|| "determinism probe globals: install failed".to_string())?;
    Ok(())
}
