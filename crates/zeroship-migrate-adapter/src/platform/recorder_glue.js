// Phase F Stage 4a — the authoring glue that runs the STANDALONE `zero-migrate`
// v1 recorder inside zeroship-runtime's V8 isolate to emit an `ir_version:1` IR
// envelope. Shared mechanism with the S2 test glue (`tests/stage2_recorder.js`),
// re-homed here as the production authoring seam, emitting on `__zsPlatformIR`.
//
// Import the platform migration under the fixed specifier `__migration__.js`,
// import the recorder seam `{ __begin, __drain }` from `@zeroship/migrate` (mapped
// to the standalone `dist/embedded-recorder.js`, the current v1 DSL/recorder), run
// `up()` under a fresh ambient recorder, drain the op list, and stamp
// `ir_version: 1` — the version the published engine's fail-closed load gate
// accepts. The recorder does NOT compute a checksum and does NOT set `owner_app`:
// those are Rust-owned provenance/integrity fields.

import * as userMod from "./__migration__.js";
import { __begin, __drain } from "@zeroship/migrate";

// Resolve `up()` (mandatory) from `export function up()` or `export default { up }`.
function resolveUp(mod) {
  const def = mod && mod.default;
  let up = typeof mod.up === "function" ? mod.up : undefined;
  if (!up && def && typeof def === "object" && typeof def.up === "function") {
    up = def.up;
  }
  if (!up) {
    throw new Error(
      "platform recorder: the migration module exports no `up()` function " +
        "(named export `up` or `default.up`)",
    );
  }
  return up;
}

// The migration name: explicit `name` export → `default.name` → the host-supplied
// filename-derived label.
function resolveName(mod) {
  if (typeof mod.name === "string" && mod.name.length > 0) return mod.name;
  const def = mod && mod.default;
  if (def && typeof def.name === "string" && def.name.length > 0) return def.name;
  return (globalThis.__zsMigrationName && String(globalThis.__zsMigrationName)) || "migration";
}

// Record the `up` phase: install a FRESH ambient recorder, run the phase so the
// op-functions record into it, then drain.
function recordUp(up) {
  __begin("up");
  up();
  return __drain();
}

try {
  const up = resolveUp(userMod);
  const ops = recordUp(up);
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
