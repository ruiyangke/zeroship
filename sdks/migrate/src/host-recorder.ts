// Host recorder (design §D.1) — the pure-JS half of authoring a `.ir.json`.
//
// The in-Rust V8 recorder (`crates/zeroship-migrate/src/frontend/op_recorder.js`)
// imports a creator migration + `{ __begin, __drain }` from `@zeroship/migrate`,
// runs `up()` under a fresh ambient recorder, drains the op list, and emits the
// `{ ir_version, name, ops }` ENVELOPE for the Rust host to read back. The in-V8
// isolate was only ever a *sandbox* for untrusted `up()`, never an authoring
// requirement (§D.1) — the recorder is ordinary ESM, so Node/Bun can drain the SAME
// op list purely in JS.
//
// This module is the SDK-exported equivalent of that Rust-side glue: it takes an
// already-imported migration module (the facade / a bundler resolves the `.ts`),
// runs the recorder, and returns the envelope. It deliberately DOES NOT compute the
// checksum (§D.1 point 2) and DOES NOT set `owner_app` (a provenance field the
// builder must not influence, §D.1 / the in-V8 recorder's PR4a HIGH #1): the addon
// stamps `owner_app` and folds the single authoritative `Checksum::of_ir` in Rust.
//
// `CURRENT_IR_VERSION` is a SINGLE SOURCE OF TRUTH across the boundary (§D.1 point
// 2): the addon's `irVersion()` is the floor the Rust core validates against; this
// module reads it from the addon at envelope-build time rather than hardcoding `6`
// (which both `op_recorder.js` and `model/ir.rs` did). A host that authors without a
// loaded addon may pass the version explicitly.

// FRAMEWORK-INTERNAL recorder seam: import `__begin`/`__drain` DIRECTLY from the
// recorder module (`./ops.js`) — NOT re-exported through the public `.` entry
// (`index.ts`), which deliberately omits recorder internals (the public-surface
// test enforces `__begin` stays out of the root `.d.ts`). tsup's code-SPLITTING
// (`splitting: true`) hoists `ops.ts` into ONE shared chunk that both `index.js`
// (the migration's `table()`/`t.*`) and this `host-recorder.js` import — so they
// resolve to the SAME ambient recorder singleton, which is the load-bearing
// requirement (a duplicated module would drain an empty op list). This is the
// pure-JS peer of the in-V8 `op_recorder.js` seam.
import { __begin, __drain } from "./ops.js";

/** The pure-JS `.ir.json` envelope the addon lowers (§D.1). Note: NO `owner_app`,
 *  NO `checksum` — both are Rust-owned provenance/integrity fields. */
export interface IrEnvelope {
  /** The IR-format version, sourced from the addon's `irVersion()` (§D.1). */
  ir_version: number;
  /** The migration name: explicit `name` export → `default.name` → the supplied
   *  fallback label. */
  name: string;
  /** The recorded canonical op list drained from the DSL recorder. */
  ops: unknown[];
}

/** The two accepted migration-module shapes (mirrors the deploy contract's
 *  discovery order, exactly as the in-V8 `resolveMigration` does). */
export interface MigrationModule {
  up?: () => void;
  down?: () => void;
  name?: string;
  default?: { up?: () => void; down?: () => void; name?: string };
}

/**
 * Resolve the migration's `up()` (mandatory) from either
 * `export function up()` or `export default { up }` (mirrors `op_recorder.js`
 * `resolveMigration`).
 */
function resolveUp(mod: MigrationModule): () => void {
  const def = mod && mod.default;
  let up = typeof mod.up === "function" ? mod.up : undefined;
  if (!up && def && typeof def === "object" && typeof def.up === "function") {
    up = def.up;
  }
  if (!up) {
    throw new Error(
      "host recorder: the migration module exports no `up()` function " +
        "(named export `up` or `default.up`)",
    );
  }
  return up;
}

/**
 * Resolve the migration name: explicit `name` export → `default.name` → the
 * caller-supplied fallback (typically a filename-derived label), mirroring
 * `op_recorder.js` `resolveName`.
 */
function resolveName(mod: MigrationModule, fallback: string): string {
  if (typeof mod.name === "string" && mod.name.length > 0) return mod.name;
  const def = mod && mod.default;
  if (def && typeof def.name === "string" && def.name.length > 0) return def.name;
  return fallback && fallback.length > 0 ? fallback : "migration";
}

/**
 * Record one phase's op list: install a FRESH ambient recorder (`__begin`), call
 * the phase so the op-functions record into it, then `__drain`. Installing the
 * recorder per-phase is what makes an op-function called OUTSIDE a phase a
 * structured `OP_OUTSIDE_RECORDER` error rather than a silently-lost op (§D.1).
 *
 * IMPORTANT: `__begin`/`__drain` and the migration's `table()`/`t.*` calls MUST
 * resolve to the SAME `@zeroship/migrate` module instance (one ambient recorder
 * singleton). A bundler that duplicates the module would drain an empty list; the
 * facade/oracle imports the migration through the same resolution as this module.
 */
function recordUp(up: () => void): unknown[] {
  __begin("up");
  up();
  return __drain();
}

/** Options for {@link buildEnvelope}. */
export interface BuildEnvelopeOptions {
  /** The IR-format version. Pass the addon's `irVersion()` (the single source of
   *  truth, §D.1). Required so this module never re-hardcodes `6`. */
  irVersion: number;
  /** The filename-derived fallback name when the module declares none. */
  nameFallback?: string;
}

/**
 * Build the `.ir.json` ENVELOPE from an already-imported migration module — the
 * pure-JS §D.1 authoring path. Runs `up()` under a fresh recorder, drains the ops,
 * and stamps the caller-supplied `irVersion` (from the addon). Does NOT set
 * `owner_app` or fold a checksum — the addon does both in Rust.
 *
 * @throws the structured recorder errors (`OP_OUTSIDE_RECORDER`,
 *         `SELECTOR_NOT_TERMINATED`) verbatim, and a plain `Error` for a missing
 *         `up()`.
 */
export function buildEnvelope(
  mod: MigrationModule,
  opts: BuildEnvelopeOptions,
): IrEnvelope {
  const up = resolveUp(mod);
  const ops = recordUp(up);
  return {
    ir_version: opts.irVersion,
    name: resolveName(mod, opts.nameFallback ?? "migration"),
    ops,
  };
}

/**
 * Dynamic-import a migration module from a path, then {@link buildEnvelope}. The
 * path must resolve to a module the runtime can import directly (an already-built
 * `.js`, or a `.ts` under Bun / a Node `.ts` loader). For an arbitrary `.ts` on
 * plain Node, pre-bundle it (esbuild) so `@zeroship/migrate` resolves to THIS
 * package's dist (one recorder instance) and hand the resulting module here.
 */
export async function buildEnvelopeFromPath(
  path: string,
  opts: BuildEnvelopeOptions,
): Promise<IrEnvelope> {
  const mod = (await import(path)) as MigrationModule;
  const fallback = opts.nameFallback ?? deriveNameFromPath(path);
  return buildEnvelope(mod, { irVersion: opts.irVersion, nameFallback: fallback });
}

/** Derive a migration label from a file path (basename without extension). */
export function deriveNameFromPath(path: string): string {
  const base = path.split(/[\\/]/).pop() ?? path;
  return base.replace(/\.(ts|mts|cts|js|mjs|cjs)$/i, "") || "migration";
}
