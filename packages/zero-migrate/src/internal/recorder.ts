// Host recorder — the pure-JS half of authoring an IR envelope.
//
// It imports a creator migration + `{ __begin, __drain }` from `zero-migrate`,
// runs one `schema()` or `data()` forward phase under a fresh ambient recorder,
// drains the op list, and emits the `{ ir_version, name, ops }` ENVELOPE for the
// Rust host to read back. A data migration's optional `inverse()` is recorded in
// a second, independent pass and stays on the recorder-only return value until
// the IR and Rust host grow that field together.
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

type MigrationPhase = () => unknown;
type PhaseMember = "schema" | "data" | "inverse" | "up" | "down";
type RecordablePhase = "schema" | "data" | "inverse";

/** Fields that may arrive as named exports or members of the default export.
 *
 * `up` and `down` remain visible here only so the big-bang protocol can refuse
 * them with an instructive error. `irreversible` is deliberately `unknown`: an
 * imported JavaScript module can carry any value, and boolean `true` in
 * particular must reach the reason validator rather than be hidden by the type. */
interface MigrationMembers {
  schema?: MigrationPhase;
  data?: MigrationPhase;
  inverse?: MigrationPhase;
  irreversible?: unknown;
  up?: MigrationPhase;
  down?: MigrationPhase;
  name?: string;
}

/** The two accepted migration-module locations, named exports and `default.*`.
 * This is the shape a module MAY arrive carrying, not the narrower public
 * authoring type: invalid and legacy members must remain observable so the
 * recorder can refuse them. */
export interface MigrationModule extends MigrationMembers {
  default?: MigrationMembers;
}

interface ResolvedSchemaMigration {
  phase: "schema";
  forward: MigrationPhase;
}

interface ResolvedReversibleDataMigration {
  phase: "data";
  forward: MigrationPhase;
  reverse: { kind: "inverse"; phase: MigrationPhase };
}

interface ResolvedIrreversibleDataMigration {
  phase: "data";
  forward: MigrationPhase;
  reverse: { kind: "irreversible"; reason: string };
}

type ResolvedMigration =
  | ResolvedSchemaMigration
  | ResolvedReversibleDataMigration
  | ResolvedIrreversibleDataMigration;

function defaultMembers(mod: MigrationModule): MigrationMembers | undefined {
  const def = mod.default;
  return def !== null && typeof def === "object" ? def : undefined;
}

/** Resolve a callable member with the legacy recorder's discovery order: a
 * named export wins, then the same member on the default-exported object. */
function resolvePhase(mod: MigrationModule, member: PhaseMember): MigrationPhase | undefined {
  const named = mod[member];
  if (typeof named === "function") return named;
  const fallback = defaultMembers(mod)?.[member];
  return typeof fallback === "function" ? fallback : undefined;
}

/** Resolve the reason value without filtering by type: validating a malformed
 * value is part of the protocol. As with phase functions, the named export wins. */
function resolveIrreversible(mod: MigrationModule): unknown {
  if (mod.irreversible !== undefined) return mod.irreversible;
  return defaultMembers(mod)?.irreversible;
}

/** Resolve and validate the module shape before any author code executes. The
 * order intentionally gives obsolete members and malformed reverse declarations
 * their specific migration-path errors instead of collapsing them into the
 * generic "neither schema nor data" refusal. */
function resolveMigration(mod: MigrationModule): ResolvedMigration {
  if (resolvePhase(mod, "up") !== undefined) {
    throw new Error(
      "host recorder: up() is no longer supported; use schema() for DDL or data() for DML, in separate migration modules",
    );
  }

  if (resolvePhase(mod, "down") !== undefined) {
    // `down()` was always refused because this recorder never captured it: the
    // written body disappeared while rollback ran an engine-synthesised inverse.
    // `inverse()` is different precisely because it is recorded through the DSL
    // seam below, making the reverse checksummable and lintable instead of an
    // opaque body that downstream code cannot inspect.
    const error = new Error(
      "migration authors a down() function, which the recorder does not capture; rollback runs the engine's synthesised inverse, so the authored body would never execute; inverse() on a data() migration is the supported way to declare a recorded reverse",
    ) as Error & { code: string; suggested_fix: string };
    error.code = "AUTHORED_DOWN_UNSUPPORTED";
    error.suggested_fix =
      "remove down(); use inverse() on a data() migration when a recorded reverse exists";
    throw error;
  }

  const schema = resolvePhase(mod, "schema");
  const data = resolvePhase(mod, "data");
  const inverse = resolvePhase(mod, "inverse");
  const irreversible = resolveIrreversible(mod);
  const hasIrreversible = irreversible !== undefined;

  if (schema !== undefined && data !== undefined) {
    throw new Error(
      "host recorder: schema and data changes must be separate migrations; export schema() and data() from different migration modules",
    );
  }

  if (data === undefined && (inverse !== undefined || hasIrreversible)) {
    throw new Error(
      "host recorder: inverse() and irreversible may only be declared on a data() migration",
    );
  }

  if (schema === undefined && data === undefined) {
    throw new Error(
      "host recorder: the migration module exports neither schema() nor data(); export exactly one of schema() or data()",
    );
  }

  if (schema !== undefined) return { phase: "schema", forward: schema };

  // The missing-forward refusal above proves this branch has a data phase, but
  // state it directly so the strict type does not rely on a non-null assertion.
  if (data === undefined) {
    throw new Error(
      "host recorder: the migration module exports neither schema() nor data(); export exactly one of schema() or data()",
    );
  }

  if (inverse !== undefined && hasIrreversible) {
    throw new Error(
      "host recorder: a data() migration cannot declare both inverse() and irreversible; they are mutually exclusive",
    );
  }

  if (inverse === undefined && !hasIrreversible) {
    throw new Error(
      "host recorder: a data() migration must declare exactly one of inverse() or irreversible with a non-empty reason",
    );
  }

  if (inverse !== undefined) {
    return {
      phase: "data",
      forward: data,
      reverse: { kind: "inverse", phase: inverse },
    };
  }

  if (typeof irreversible !== "string" || irreversible.trim().length === 0) {
    throw new Error(
      "host recorder: irreversible must be a non-empty string explaining why this data() migration cannot be reversed; boolean true is not a reason, and lint/status need that text during a rollback decision",
    );
  }

  return {
    phase: "data",
    forward: data,
    reverse: { kind: "irreversible", reason: irreversible },
  };
}

/**
 * Resolve the migration name: explicit `name` export → `default.name` → the
 * caller-supplied fallback (typically a filename-derived label).
 */
export function resolveMigrationName(mod: MigrationModule, fallback: string): string {
  if (typeof mod.name === "string" && mod.name.length > 0) return mod.name;
  const def = defaultMembers(mod);
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
function recordPhase(phase: RecordablePhase, author: MigrationPhase): unknown[] {
  __begin();
  try {
    const result = author();
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
        `migration ${phase}() must be synchronous; promises and async functions are not supported`,
      ) as Error & { code: string; suggested_fix: string };
      error.code = "ASYNC_PHASE_UNSUPPORTED";
      error.suggested_fix =
        `remove async/await and author every migration operation synchronously inside ${phase}()`;
      throw error;
    }
    return __drain();
  } catch (error) {
    __abort();
    throw error;
  }
}

/** Options for {@link recordMigration} and {@link buildEnvelope}. */
export interface BuildEnvelopeOptions {
  /** The IR-format version. Pass the addon's `irVersion()` (the single source of
   *  truth). Required so this module never re-hardcodes a version. */
  irVersion: number;
  /** The filename-derived fallback name when the module declares none. */
  nameFallback?: string;
}

/** Recorder-owned output that keeps reverse authoring beside, but never inside,
 * the Rust-bound envelope. A later coordinated IR/Rust change can decide how to
 * transport these fields without changing today's deny-unknown-fields JSON. */
export interface RecordedMigration {
  /** Exactly the historical `{ ir_version, name, ops }` JSON envelope. */
  envelope: IrEnvelope;
  /** The independently recorded `inverse()` stream for reversible data. */
  inverseOps?: unknown[];
  /** The author's exact reason for an irreversible data migration. */
  irreversible?: string;
}

/**
 * Record an already-imported migration module. `schema()` and `data()` each use
 * one fresh forward pass; a data `inverse()` uses a SECOND fresh pass so it never
 * executes against a database or perturbs the forward stream and its ordering.
 */
export function recordMigration(
  mod: MigrationModule,
  opts: BuildEnvelopeOptions,
): RecordedMigration {
  const migration = resolveMigration(mod);
  const ops = recordPhase(migration.phase, migration.forward);
  const envelope: IrEnvelope = {
    ir_version: opts.irVersion,
    name: resolveMigrationName(mod, opts.nameFallback ?? "migration"),
    ops,
  };

  if (migration.phase === "schema") return { envelope };
  if (migration.reverse.kind === "irreversible") {
    return { envelope, irreversible: migration.reverse.reason };
  }
  return {
    envelope,
    inverseOps: recordPhase("inverse", migration.reverse.phase),
  };
}

/**
 * Build only the unchanged Rust-bound IR envelope. Reverse metadata remains on
 * {@link recordMigration}'s wrapper and is deliberately not serialized here:
 * Rust's current `MigrationIr` denies unknown fields.
 */
export function buildEnvelope(
  mod: MigrationModule,
  opts: BuildEnvelopeOptions,
): IrEnvelope {
  return recordMigration(mod, opts).envelope;
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
