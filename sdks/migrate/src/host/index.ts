// `@zeroship/migrate/host` — the creator-facing HOST facade (design §D.3).
//
// A thin async layer over the prebuilt N-API addon (`crates/zeroship-migrate-node`,
// Phase C). The creator never sees N-API, `SeamRow`, or the `hostDriver` callback:
//
//   import { apply, plan, status, history } from "@zeroship/migrate/host";
//   await apply({ migration, ownerApp, projectSchema, driver: { kind:"postgres", url } });
//
// The flow for `apply` (§D.1/§D.3):
//   1. the pure-JS HOST RECORDER (`host-recorder.ts`) drains the migration's `up()`
//      into a `{ ir_version, name, ops }` ENVELOPE — `ir_version` sourced from the
//      addon's `irVersion()` (single source of truth); NO `owner_app`, NO checksum;
//   2. the addon's `applyIr` LOWERS the envelope in Rust (stamps `owner_app`, folds
//      the authoritative `Checksum::of_ir`), then drives `executor::apply` over the
//      chosen host driver (`driver-pg.ts`).
//
// NO `dryRun` verb in v1: the host-side shadow harness (§F) is deferred; on the
// `host-pg` addon build `backend.shadow()` is `None`, so a shadow dry-run would
// return `DryRunError::ShadowUnsupported`. `plan` (the DB-free pre-check) IS
// provided.

import { loadAddon, currentIrVersion, type MigrateAddon } from "./addon.js";
import { openPgSession, type HostDriver } from "./driver-pg.js";
import { openMysqlSession } from "./driver-mysql2.js";
import { buildEnvelope, type IrEnvelope, type MigrationModule } from "../host-recorder.js";

export { currentIrVersion } from "./addon.js";
export type { IrEnvelope, MigrationModule } from "../host-recorder.js";

/** A driver target the facade opens a pinned host session against. */
export type DriverConfig =
  | { kind: "postgres"; url: string }
  | { kind: "mysql"; url: string };

/** The dialect string the addon lower + load-verify expect. */
function dialectOf(driver: DriverConfig): "postgres" | "mysql" {
  return driver.kind;
}

/** Open the pinned host session for a driver and return its `hostDriver` callback +
 *  a `close()`. PG uses `driver-pg.ts` (connection-scoped exact-integer parsers);
 *  MySQL uses `driver-mysql2.ts`. */
async function openSession(
  driver: DriverConfig,
): Promise<{ hostDriver: HostDriver; close: () => Promise<void> }> {
  if (driver.kind === "postgres") {
    const s = await openPgSession(driver.url);
    return { hostDriver: s.hostDriver, close: s.close };
  }
  if (driver.kind === "mysql") {
    const s = await openMysqlSession(driver.url);
    return { hostDriver: s.hostDriver as HostDriver, close: s.close };
  }
  throw new Error(`@zeroship/migrate/host: unsupported driver ${JSON.stringify((driver as { kind: string }).kind)}`);
}

/** Common inputs to the host verbs. */
export interface HostApplyOptions {
  /** The migration module (an imported `.ts`/`.js` exporting `up()` or
   *  `default.up`). Resolved to an envelope by the host recorder. */
  migration: MigrationModule;
  /** The deploying app id (`app_…`) — stamped as `owner_app` + folded into the
   *  checksum by the addon (§D.1). */
  ownerApp: string;
  /** The confined project schema the lower pins ops to (§2.7). */
  projectSchema: string;
  /** The target DB driver. */
  driver: DriverConfig;
  /** The project's `{ table: owner_app }` registry (ownership check, §8.6).
   *  Defaults to `{}` (a fresh single-app project). */
  registry?: Record<string, string>;
  /** The migrator role to `SET ROLE` under (least-privilege apply). Optional. */
  migratorRole?: string;
  /** Whether destructive changes are pre-approved. Default `false`. */
  approved?: boolean;
  /** The audit `applied_by` label recorded in the journal. Default `"host"`. */
  appliedBy?: string;
  /** Override the migration's declared name (else recorder-resolved). */
  nameFallback?: string;
}

/** The parsed JSON `ApplyOutcome`. */
export interface ApplyOutcome {
  applied: string[];
  skipped: string[];
  recovered: string[];
}

/**
 * Author the envelope (pure JS) then drive the addon's host-authoring `applyIr`
 * over the chosen driver — the full §D.1/§D.3 apply. Resolves to the parsed
 * `ApplyOutcome`. The pinned session is always closed (success or throw).
 */
export async function apply(opts: HostApplyOptions): Promise<ApplyOutcome> {
  const addon = loadAddon();
  const envelope = authorEnvelope(addon, opts.migration, opts.nameFallback);
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    const json = await addon.applyIr(
      hostDriver as never,
      opts.ownerApp,
      opts.projectSchema,
      opts.migratorRole ?? null,
      dialectOf(opts.driver),
      JSON.stringify(opts.registry ?? {}),
      JSON.stringify(envelope),
      opts.approved ?? false,
      opts.appliedBy ?? "host",
    );
    return JSON.parse(json) as ApplyOutcome;
  } finally {
    await close();
  }
}

/** Options for {@link plan} — the DB-free pre-check (no driver needed). */
export interface HostPlanOptions {
  migration: MigrationModule;
  ownerApp: string;
  dialect?: "postgres" | "mysql" | "sqlite";
  registry?: Record<string, string>;
  nameFallback?: string;
}

/** A DB-free plan pre-check verdict (the load-verify report, §C.5). */
export interface PlanReport {
  ok: boolean;
  ir_version?: number;
  op_count?: number;
  error?: string;
  /** The authored envelope (ops), for inspection. */
  envelope: IrEnvelope;
}

/**
 * The DB-free pre-check (§D.3): author the envelope (pure JS), then run the addon's
 * sync `loadVerify` (fail-closed structural + confinement + ownership validation,
 * no DB). Returns the verdict + the authored ops. This is the fast pre-apply gate;
 * `dryRun` (the full shadow verification) is deferred (§F).
 */
export function plan(opts: HostPlanOptions): PlanReport {
  const addon = loadAddon();
  const envelope = authorEnvelope(addon, opts.migration, opts.nameFallback);
  const report = JSON.parse(
    addon.loadVerify(
      JSON.stringify(envelope),
      opts.ownerApp,
      opts.dialect ?? "postgres",
      JSON.stringify(opts.registry ?? {}),
    ),
  ) as { ok: boolean; ir_version?: number; op_count?: number; error?: string };
  return { ...report, envelope };
}

/** Options for {@link status}/{@link history}. */
export interface HostStatusOptions {
  /** The migrations to reconcile against the journal. Each is an authored
   *  envelope's module; the facade lowers via the addon. For the create-first
   *  posture, pass the same modules `apply` used. */
  migrations?: MigrationModule[];
  ownerApp: string;
  projectSchema: string;
  driver: DriverConfig;
  registry?: Record<string, string>;
  nameFallback?: string;
}

/**
 * `status` (§C.5) — reconcile the supplied migrations against the live journal over
 * the host driver. Returns the parsed `MigrationStatus` JSON.
 *
 * NOTE: the Phase-C `status`/`history` addon entries take a pre-lowered
 * `Vec<Migration>` JSON; lowering an arbitrary module needs the addon's lower. For
 * v1 the facade supports the empty-journal / no-migrations status query (the
 * "pending" flow) by passing `[]`; lowering N modules to `Vec<Migration>` for a
 * populated status query reuses `applyIr`'s lower and is a small follow-up. The
 * oracle drives `status` with `[]` (empty journal) to prove the read path.
 */
export async function status(
  opts: HostStatusOptions,
): Promise<{ current_version: string | null; applied: string[]; pending: string[]; rolled_back: string[] }> {
  const addon = loadAddon();
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    const migrationsJson = "[]"; // v1: the read path / empty-journal flow (see doc).
    const json = await addon.status(
      hostDriver as never,
      opts.projectSchema,
      opts.projectSchema,
      migrationsJson,
    );
    return JSON.parse(json);
  } finally {
    await close();
  }
}

/** `history` (§C.5) — the journal audit trail over the host driver. Returns the
 *  parsed `Vec<HistoryEvent>` JSON. */
export async function history(
  opts: Omit<HostStatusOptions, "migrations">,
): Promise<Array<Record<string, unknown>>> {
  const addon = loadAddon();
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    const json = await addon.history(hostDriver as never, opts.projectSchema, opts.projectSchema);
    return JSON.parse(json);
  } finally {
    await close();
  }
}

/**
 * `generate` (§D.3) — the schema-diff → IR autogenerate step. This is a sync,
 * DB-free differ that compares a desired `t.*` schema snapshot to a live snapshot;
 * it is NOT yet wired through the host addon (the addon exposes no `generate`
 * entry in Phase C). Deferred — throws an explicit error rather than a fake result.
 */
export function generate(): never {
  throw new Error(
    "@zeroship/migrate/host: `generate` (schema-diff autogenerate) is not wired in v1 " +
      "— the Phase-C addon exposes no generate entry. Author migrations with the DSL and " +
      "use `apply`/`plan`.",
  );
}

/** Author the `{ ir_version, name, ops }` envelope from a migration module, sourcing
 *  `ir_version` from the addon (single source of truth, §D.1). */
function authorEnvelope(
  addon: MigrateAddon,
  migration: MigrationModule,
  nameFallback?: string,
): IrEnvelope {
  return buildEnvelope(migration, { irVersion: addon.irVersion(), nameFallback });
}
