// Phase F Stage 4a — the authoring glue that runs the STANDALONE `zero-migrate`
// v1 recorder inside zeroship-runtime's V8 isolate to emit an `ir_version:1` IR
// envelope. Shared mechanism with the S2 test glue (`tests/stage2_recorder.js`),
// re-homed here as the production authoring seam, emitting on `__zsPlatformIR`.
//
// Import the platform migration under the fixed specifier `__migration__.js`,
// import the recorder seam `{ __begin, __drain }` from `@zeroship/migrate` (mapped
// to the standalone `dist/embedded-recorder.js`, the current v1 DSL/recorder), run
// `schema()` under a fresh ambient recorder, drain the op list, and stamp
// `ir_version: 1` — the version the published engine's fail-closed load gate
// accepts. The recorder does NOT compute a checksum and does NOT set `owner_app`:
// those are Rust-owned provenance/integrity fields.
//
// The DDL phase member is `schema()`, the same member the host recorder
// resolves (packages/zero-migrate/src/internal/recorder.ts:177). `up()` is
// refused here by name with that recorder's message, so a migration left on the
// obsolete shape fails loudly at the authoring seam instead of recording nothing.

import * as userMod from "./__migration__.js";
import { __begin, __drain } from "@zeroship/migrate";

// Resolve a phase member from `export function <member>()` or
// `export default { <member> }`, in that order.
function resolveMember(mod, member) {
  if (typeof mod[member] === "function") return mod[member];
  const def = mod && mod.default;
  // Returned UNBOUND, exactly as the host recorder does
  // (packages/zero-migrate/src/internal/recorder.ts:137-142), so a phase that
  // reaches for `this` fails the same way on both seams instead of only one.
  if (def && typeof def === "object" && typeof def[member] === "function") {
    return def[member];
  }
  return undefined;
}

// Resolve `schema()` (mandatory), refusing the obsolete `up()` shape by name.
function resolveSchema(mod) {
  if (resolveMember(mod, "up") !== undefined) {
    throw new Error(
      "platform recorder: up() is no longer supported; use schema() for DDL or " +
        "data() for DML, in separate migration modules",
    );
  }
  const schema = resolveMember(mod, "schema");
  if (!schema) {
    throw new Error(
      "platform recorder: the migration module exports no `schema()` function " +
        "(named export `schema` or `default.schema`)",
    );
  }
  return schema;
}

// The migration name: explicit `name` export → `default.name` → the host-supplied
// filename-derived label.
function resolveName(mod) {
  if (typeof mod.name === "string" && mod.name.length > 0) return mod.name;
  const def = mod && mod.default;
  if (def && typeof def.name === "string" && def.name.length > 0) return def.name;
  return (globalThis.__zsMigrationName && String(globalThis.__zsMigrationName)) || "migration";
}

// Record the `schema` phase: install a FRESH ambient recorder, run the phase so
// the op-functions record into it, then drain.
function recordSchema(schema) {
  __begin();
  schema();
  return __drain();
}

try {
  const schema = resolveSchema(userMod);
  const ops = recordSchema(schema);
  const envelope = {
    ir_version: 1,
    name: resolveName(userMod),
    ops,
  };
  globalThis.__zsPlatformIR = JSON.stringify({ ok: true, ir: envelope });
} catch (e) {
  globalThis.__zsPlatformIR = JSON.stringify({
    ok: false,
    error: e && e.message ? e.message : String(e),
  });
}

export default {};
