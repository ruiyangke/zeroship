// Host recorder — the pure-JS half of authoring an IR envelope.
//
// It imports a creator migration + `{ __begin, __drain }` from `zero-migrate`,
// runs `up()` under a fresh ambient recorder, drains the op list, and emits the
// `{ ir_version, name, ops }` ENVELOPE for the Rust host to read back. The recorder
// is ordinary ESM, so Node/Bun can drain the op list purely in JS.
//
// It takes an already-imported migration module (the facade / a bundler resolves
// the `.ts`), runs the recorder, and returns the envelope. It deliberately DOES NOT
// compute the checksum and DOES NOT set `owner_app` (a provenance field the
// builder must not influence): the addon stamps `owner_app` and folds the single
// authoritative `Checksum::of_ir` in Rust.
//
// `CURRENT_IR_VERSION` is a SINGLE SOURCE OF TRUTH across the boundary: the addon's
// `irVersion()` is the floor the Rust core validates against; this module reads it
// from the addon at envelope-build time rather than hardcoding a version. A host
// that authors without a loaded addon may pass the version explicitly.

// FRAMEWORK-INTERNAL recorder seam: import `__begin`/`__drain` DIRECTLY from the
// recorder module (`./ops.js`) — NOT re-exported through the public `.` entry
// (`index.ts`), which deliberately omits recorder internals (the public-surface
// test enforces `__begin` stays out of the root `.d.ts`). tsup's code-SPLITTING
// (`splitting: true`) hoists `ops.ts` into ONE shared chunk that both `index.js`
// (the migration's `table()`/`t.*`) and this recorder module import — so they
// resolve to the SAME ambient recorder singleton, which is the load-bearing
// requirement (a duplicated module would drain an empty op list). It is exposed to
// the `zero-migrate-cli` host package via the documented `./internal/recorder`
// subpath export (the ONE sanctioned consumer) — NOT part of the public `.` API.
import { __abort, __begin, __drain } from "../ops.js";

/** The pure-JS IR envelope the addon lowers. Note: NO `owner_app`,
 *  NO `checksum` — both are Rust-owned provenance/integrity fields. */
export interface IrEnvelope {
  /** The IR-format version, sourced from the addon's `irVersion()`. */
  ir_version: number;
  /** The migration name: explicit `name` export → `default.name` → the supplied
   *  fallback label. */
  name: string;
  /** The recorded canonical op list drained from the DSL recorder. */
  ops: unknown[];
}

/** The two accepted migration-module shapes (mirrors the deploy contract's
 *  discovery order). */
export interface MigrationModule {
  up?: () => void;
  down?: () => void;
  name?: string;
  default?: { up?: () => void; down?: () => void; name?: string };
}

/**
 * Resolve the migration's `up()` (mandatory) from either
 * `export function up()` or `export default { up }`.
 */
function resolveUp(mod: MigrationModule): () => unknown {
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
 * caller-supplied fallback (typically a filename-derived label).
 */
export function resolveMigrationName(mod: MigrationModule, fallback: string): string {
  if (typeof mod.name === "string" && mod.name.length > 0) return mod.name;
  const def = mod && mod.default;
  if (def && typeof def.name === "string" && def.name.length > 0) return def.name;
  return fallback && fallback.length > 0 ? fallback : "migration";
}

/**
 * Record one phase's op list: install a FRESH ambient recorder (`__begin`), call
 * the phase so the op-functions record into it, then `__drain`. Installing the
 * recorder per-phase is what makes an op-function called OUTSIDE a phase a
 * structured `OP_OUTSIDE_RECORDER` error rather than a silently-lost op.
 *
 * IMPORTANT: `__begin`/`__drain` and the migration's `table()`/`t.*` calls MUST
 * resolve to the SAME `zero-migrate` module instance (one ambient recorder
 * singleton). A bundler that duplicates the module would drain an empty list; the
 * facade/oracle imports the migration through the same resolution as this module.
 */
function recordUp(up: () => unknown): unknown[] {
  __begin("up");
  try {
    const result = up();
    if (
      result !== null &&
      (typeof result === "object" || typeof result === "function") &&
      typeof (result as { then?: unknown }).then === "function"
    ) {
      // The promise cannot be cancelled. Observe any eventual rejection so a
      // post-await authoring call does not become an unhandled rejection after
      // this synchronous validation error has already been reported.
      void Promise.resolve(result).catch(() => undefined);
      const error = new Error(
        "migration up() must be synchronous; promises and async functions are not supported",
      ) as Error & { code: string; suggested_fix: string };
      error.code = "ASYNC_UP_UNSUPPORTED";
      error.suggested_fix =
        "remove async/await and author every migration operation synchronously inside up()";
      throw error;
    }
    return __drain();
  } catch (error) {
    __abort();
    throw error;
  }
}

/** Options for {@link buildEnvelope}. */
export interface BuildEnvelopeOptions {
  /** The IR-format version. Pass the addon's `irVersion()` (the single source of
   *  truth). Required so this module never re-hardcodes a version. */
  irVersion: number;
  /** The filename-derived fallback name when the module declares none. */
  nameFallback?: string;
}

/**
 * Build the IR envelope from an already-imported migration module — the
 * pure-JS authoring path. Runs `up()` under a fresh recorder, drains the ops,
 * and stamps the caller-supplied `irVersion` (from the addon). Does NOT set
 * `owner_app` or fold a checksum — the addon does both in Rust.
 *
 * @throws structured recorder errors (`OP_OUTSIDE_RECORDER`,
 *         `SELECTOR_NOT_TERMINATED`, `ASYNC_UP_UNSUPPORTED`) verbatim, and a
 *         plain `Error` for a missing `up()`.
 */
export function buildEnvelope(
  mod: MigrationModule,
  opts: BuildEnvelopeOptions,
): IrEnvelope {
  const up = resolveUp(mod);
  const ops = recordUp(up);
  return {
    ir_version: opts.irVersion,
    name: resolveMigrationName(mod, opts.nameFallback ?? "migration"),
    ops,
  };
}

/**
 * Dynamic-import a migration module from a path, then {@link buildEnvelope}. The
 * path must resolve to a module the runtime can import directly (an already-built
 * `.js`, or a `.ts` under Bun / a Node `.ts` loader). For an arbitrary `.ts` on
 * plain Node, pre-bundle it (esbuild) so `zero-migrate` resolves to THIS
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
