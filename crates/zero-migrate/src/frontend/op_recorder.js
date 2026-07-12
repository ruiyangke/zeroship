// op.* recorder adapter — the JS half of the PR1 anti-drift gate (design §2.5 /
// PR1 "skeletal JS builder + round-trip fixtures").
//
// Evaluated inside the V8 host runtime's V8 sandbox. Imports the (already-bundled,
// self-contained) creator migration module under the fixed specifier
// `__migration__.js` — a module that does
// `import { createTable, addColumn, … } from "@zeroship/migrate"` and exports a
// parameterless `up()` (and optionally `down()`). It invokes `up()`, drains the
// recorded op list from the DSL buffer, and emits the `.ir.json` ENVELOPE
// (`ir_version` + `name` + `ops` + the optional facets) on `globalThis.__zsOpIR`
// for the Rust host to read back.
//
// The JS side does NOT compute the checksum (§2.4 point 2): it emits ops; the Rust
// engine folds the single authoritative `Checksum::of_ir`. The round-trip gate
// (`tests/op_round_trip.rs`) asserts `Checksum::of_ir(JS-emitted ops) ==
// Checksum::of_ir(the committed-fixture ops re-canonicalized by Rust)` — value
// equality, invariant under any JCS-formatting difference between the two
// serializers (§2.5).

import * as userMod from "./__migration__.js";
import { __begin, __drain } from "zero-migrate";

// Resolve the migration's `up()` (mandatory) + `down()` (optional). Two accepted
// shapes, mirroring the deploy contract's discovery order:
//   1. `export function up()` / `export function down()`
//   2. `export default { up, down }`
function resolveMigration(mod) {
  const def = mod && mod.default;
  let up = (typeof mod.up === "function") ? mod.up : undefined;
  let down = (typeof mod.down === "function") ? mod.down : undefined;
  if (def && typeof def === "object") {
    if (!up && typeof def.up === "function") up = def.up;
    if (!down && typeof def.down === "function") down = def.down;
  }
  if (!up) {
    throw new Error(
      "op.* recorder: the migration module exports no `up()` function " +
        "(named export `up` or `default.up`)",
    );
  }
  return { up, down };
}

// The migration name: an explicit `export const name`, else `default.name`, else
// the host-supplied filename-derived label (a name omitted from the module records
// the filename-derived label — PR3 test).
function resolveName(mod) {
  if (typeof mod.name === "string" && mod.name.length > 0) return mod.name;
  const def = mod && mod.default;
  if (def && typeof def.name === "string" && def.name.length > 0) return def.name;
  return (globalThis.__zsMigrationName && String(globalThis.__zsMigrationName)) || "migration";
}

// Record one phase's op list: install a FRESH ambient recorder (§3.1), call the
// phase so the op-functions record into it, then drain. Returns `[]` for an
// absent phase. Installing the recorder per-phase is what makes calling an
// op-function OUTSIDE a phase (module top level / after the phase returns) a
// structured `OP_OUTSIDE_RECORDER` error rather than a silently-lost op.
function recordPhase(phase) {
  if (typeof phase !== "function") return [];
  __begin("up");
  phase();
  return __drain();
}

try {
  const mod = userMod;
  const { up } = resolveMigration(mod);
  const ops = recordPhase(up);
  // SECURITY (PR4a code-critic HIGH #1): the recorder does NOT read owner_app from
  // any JS-reachable global. `owner_app` is a tenant-identifying field folded into
  // the authoritative Checksum::of_ir provenance — untrusted up() must not be able to
  // influence it. The Rust child STAMPS owner_app onto this envelope after eval, from
  // the server-injected, ownership-cross-checked app_id. The JS half only emits ops.
  const envelope = {
    ir_version: 6,
    name: resolveName(mod),
    ops,
  };
  globalThis.__zsOpIR = JSON.stringify({ ok: true, ir: envelope });
} catch (e) {
  globalThis.__zsOpIR = JSON.stringify({
    ok: false,
    error: (e && e.message) ? e.message : String(e),
  });
}

export default {};
