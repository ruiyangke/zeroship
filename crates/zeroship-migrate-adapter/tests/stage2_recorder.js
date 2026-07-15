// Phase F Stage 2 — the authoring glue that runs the STANDALONE `zero-migrate`
// recorder inside zeroship-runtime's V8 isolate to emit an `ir_version:1` IR
// envelope.
//
// This is the v1 twin of the in-tree `crates/zeroship-migrate/src/frontend/
// op_recorder.js` (which authors v6): the mechanism is identical — import the
// creator migration under the fixed specifier `__migration__.js`, import the
// recorder seam `{ __begin, __drain }` from `@zeroship/migrate`, run `up()` under
// a fresh ambient recorder, drain the op list, and emit the envelope on a global
// for the Rust host to read back. The ONE difference is the module graph maps
// `@zeroship/migrate` to the STANDALONE `dist/embedded-recorder.js` (the current
// v1 DSL/recorder), and the envelope stamps `ir_version: 1` — the version the
// published engine's fail-closed load gate accepts.
//
// The recorder does NOT compute a checksum and does NOT set `owner_app`: those
// are Rust-owned provenance/integrity fields (the engine folds `Checksum::of_ir`
// and stamps the server-supplied owner). The JS half only emits ops.

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
      "stage2 recorder: the migration module exports no `up()` function " +
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

// Record one phase: install a FRESH ambient recorder, run the phase so the
// op-functions record into it, then drain. Installing per-phase is what makes an
// op-function called OUTSIDE a phase a structured recorder error rather than a
// silently-lost op.
function recordUp(up) {
  __begin("up");
  up();
  return __drain();
}

try {
  const up = resolveUp(userMod);
  const ops = recordUp(up);
  // `ir_version: 1` — the version the PUBLISHED zero-migrate load gate accepts
  // (fail-closed: it rejects ir_version > 1). The standalone recorder's op wire
  // shape is v1 by construction.
  const envelope = {
    ir_version: 1,
    name: resolveName(userMod),
    ops,
  };
  globalThis.__zsStage2IR = JSON.stringify({ ok: true, ir: envelope });
} catch (e) {
  globalThis.__zsStage2IR = JSON.stringify({
    ok: false,
    error: e && e.message ? e.message : String(e),
  });
}

export default {};
