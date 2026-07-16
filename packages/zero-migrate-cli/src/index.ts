// `zero-migrate-cli` — the creator-facing HOST runtime.
//
// A thin async layer over the prebuilt N-API addon (`crates/zero-migrate-node`).
// The creator never sees N-API, `driver::Row`, or the `hostDriver`
// callback:
//
//   import { apply, plan, status, history, validate } from "zero-migrate-cli";
//   await apply({ migration, ownerApp, projectSchema, driver: { kind:"postgres", url } });
//
// The flow for `apply`:
//   1. the pure-JS RECORDER (`zero-migrate/internal/recorder`, from the DSL package)
//      drains the migration's `up()` into a `{ ir_version, name, ops }` ENVELOPE —
//      `ir_version` sourced from the addon's `irVersion()` (single source of truth);
//      NO `owner_app`, NO checksum;
//   2. the addon's `applyIr` LOWERS the envelope in Rust (stamps `owner_app`, folds
//      the authoritative `Checksum::of_ir`), then drives `executor::apply` over the
//      chosen host driver (`driver-pg.ts` / `driver-mysql2.ts`).
//
// NO `dryRun` verb in v1: the host-side shadow harness is deferred; on the
// `host-pg` addon build `backend.shadow()` is `None`, so a shadow dry-run would
// return `DryRunError::ShadowUnsupported`. `plan`/`validate` (the DB-free
// pre-checks) ARE provided.

import {
  loadAddon,
  currentIrVersion,
  type MigrateAddon,
  type AddonHostDriver,
  type ApplyReply,
  type StatusReply,
  type HistoryReply,
  type LoadVerifyReply,
} from "./addon.js";
import { openPgSession } from "./driver-pg.js";
import { openMysqlSession } from "./driver-mysql2.js";
import {
  buildEnvelope,
  type IrEnvelope,
  type MigrationModule,
} from "zero-migrate/internal/recorder";

export { currentIrVersion } from "./addon.js";
export type { IrEnvelope, MigrationModule } from "zero-migrate/internal/recorder";

/** A driver target the facade opens a pinned host session against.
 *
 *  Both NETWORK dialects ride the SAME `SqlSession` seam: `postgres` (`pg`) and
 *  `mysql` (`mysql2`). Each dialect's lock / journal / placeholder SQL lives in
 *  its own `MigrationBackend` (`PostgresBackend` — `pg_advisory_lock`, `SET ROLE`;
 *  `MysqlBackend` — `GET_LOCK`, `?` placeholders), and the addon selects the
 *  backend from the `dialect` string. SQLite is NOT a host driver — it runs
 *  in-process via rusqlite and never crosses the seam. */
export type DriverConfig =
  | { kind: "postgres"; url: string }
  | { kind: "mysql"; url: string };

/** The dialect string the addon lower + load-verify + backend-select expect. */
function dialectOf(driver: DriverConfig): "postgres" | "mysql" {
  return driver.kind;
}

/** Open the pinned host session for a driver and return its `hostDriver` callback +
 *  a `close()`. PG uses `driver-pg.ts` (connection-scoped exact-integer parsers);
 *  MySQL uses `driver-mysql2.ts` (real `mysql2/promise`, BIGINT/DECIMAL
 *  as exact strings). Both open sessions return the SAME `AddonHostDriver` shape (the
 *  generated `JsRequest`/`JsReply`/`JsError` DTOs) — no cast. */
async function openSession(
  driver: DriverConfig,
): Promise<{ hostDriver: AddonHostDriver; close: () => Promise<void> }> {
  if (driver.kind === "postgres") {
    const s = await openPgSession(driver.url);
    return { hostDriver: s.hostDriver, close: s.close };
  }
  if (driver.kind === "mysql") {
    const s = await openMysqlSession(driver.url);
    return { hostDriver: s.hostDriver, close: s.close };
  }
  throw new Error(
    `zero-migrate-cli: unsupported driver ${JSON.stringify((driver as { kind: string }).kind)}`,
  );
}

/** Common inputs to the host verbs. */
export interface HostApplyOptions {
  /** The migration module (an imported `.ts`/`.js` exporting `up()` or
   *  `default.up`). Resolved to an envelope by the recorder. */
  migration: MigrationModule;
  /** The deploying app id (`app_…`) — stamped as `owner_app` + folded into the
   *  checksum by the addon. */
  ownerApp: string;
  /** The confined project schema the lower pins ops to. */
  projectSchema: string;
  /** The target DB driver. */
  driver: DriverConfig;
  /** The project's `{ table: owner_app }` registry (ownership check).
   *  Defaults to `{}` (a fresh single-app project). */
  registry?: Record<string, string>;
  /** Optional table-shape policy ceiling enforced while validating the migration. */
  policyCeiling?: string;
  /** The migrator role to `SET ROLE` under (least-privilege apply). Optional. */
  migratorRole?: string;
  /** Whether destructive changes are pre-approved. Default `false`. */
  approved?: boolean;
  /** The audit `applied_by` label recorded in the journal. Default `"host"`. */
  appliedBy?: string;
  /** Override the migration's declared name (else recorder-resolved). */
  nameFallback?: string;
}

/** The typed `applyIr` reply — re-exported from the generated addon DTOs. */
export type ApplyOutcome = ApplyReply;

/**
 * Author the envelope (pure JS) then drive the addon's host-authoring `applyIr`
 * over the chosen driver — the full apply. Resolves to the typed
 * `ApplyReply`. The pinned session is always closed (success or throw).
 */
export async function apply(opts: HostApplyOptions): Promise<ApplyOutcome> {
  const addon = loadAddon();
  const envelope = authorEnvelope(addon, opts.migration, opts.nameFallback);
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    // The verb boundary is TYPED: pass an `ApplyRequest`, get an
    // `ApplyReply` — no JSON stringify/parse. The `envelope` crosses as a JS value.
    return await addon.applyIr(hostDriver, {
      ownerApp: opts.ownerApp,
      projectSchema: opts.projectSchema,
      migratorRole: opts.migratorRole,
      dialect: dialectOf(opts.driver),
      registry: opts.registry ?? {},
      envelope,
      policyCeiling: opts.policyCeiling,
      approved: opts.approved ?? false,
      appliedBy: opts.appliedBy ?? "host",
    });
  } finally {
    await close();
  }
}

/** Options for completing or aborting a PostgreSQL online column rename. */
export interface ResolvePendingOptions {
  /** The app id that authored the original rename. */
  ownerApp: string;
  /** The project schema containing the renamed table. */
  projectSchema: string;
  /** Pending version returned by `apply()` or `status()`. */
  pendingVersion: string;
  /** `apply` keeps the new column; `abort` keeps the old column. */
  action: "apply" | "abort";
  /** Online rename resolution is currently available on PostgreSQL. */
  driver: Extract<DriverConfig, { kind: "postgres" }>;
  /** Required explicit acknowledgement of the reviewed column drop. */
  approved: true;
  /** Optional least-privilege migration role. */
  migratorRole?: string;
  /** Audit label recorded in the migration journal. */
  appliedBy?: string;
}

/**
 * Finish an outstanding PostgreSQL online rename after application cutover, or
 * abort it and return to the original column. Both actions are approval-gated.
 */
export async function resolvePending(
  opts: ResolvePendingOptions,
): Promise<ApplyOutcome> {
  if (opts.driver.kind !== "postgres") {
    throw new Error(
      "zero-migrate-cli: pending-contract resolution supports only PostgreSQL online renames",
    );
  }
  if (opts.approved !== true) {
    throw new Error(
      "zero-migrate-cli: pending-contract resolution requires explicit approval",
    );
  }
  const addon = loadAddon();
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    return await addon.resolvePending(hostDriver, {
      ownerApp: opts.ownerApp,
      projectSchema: opts.projectSchema,
      migratorRole: opts.migratorRole,
      pendingVersion: opts.pendingVersion,
      action: opts.action,
      approved: true,
      appliedBy: opts.appliedBy ?? "host",
    });
  } finally {
    await close();
  }
}

/** Options for {@link plan}/{@link validate} — the DB-free pre-checks (no driver). */
export interface HostPlanOptions {
  migration: MigrationModule;
  ownerApp: string;
  /** Confined project schema used by both offline validation and apply. */
  projectSchema?: string;
  dialect?: "postgres" | "mysql" | "sqlite";
  registry?: Record<string, string>;
  nameFallback?: string;
}

/** A DB-free plan pre-check verdict (the load-verify report). */
export interface PlanReport {
  ok: boolean;
  ir_version?: number;
  op_count?: number;
  error?: string;
  /** The authored envelope (ops), for inspection. */
  envelope: IrEnvelope;
}

/**
 * `validate` — the raw DB-free load + verify: author the envelope (pure
 * JS), then run the addon's sync `loadVerify` (fail-closed structural + confinement
 * + ownership validation, no DB). Returns the typed `LoadVerifyReply` verdict — the
 * primitive `plan` wraps into a friendlier report.
 */
export function validate(opts: HostPlanOptions): LoadVerifyReply {
  const addon = loadAddon();
  const envelope = authorEnvelope(addon, opts.migration, opts.nameFallback);
  return validateEnvelope(addon, envelope, opts);
}

/** Validate an already-authored envelope so planning never executes up() twice. */
function validateEnvelope(
  addon: MigrateAddon,
  envelope: IrEnvelope,
  opts: HostPlanOptions,
): LoadVerifyReply {
  // Typed boundary: `loadVerify` takes the IR envelope bytes + a
  // typed `Record<string,string>` registry and returns a typed `LoadVerifyReply`.
  return addon.loadVerify(
    JSON.stringify(envelope),
    opts.ownerApp,
    opts.dialect ?? "postgres",
    opts.registry ?? {},
    opts.projectSchema ?? "public",
  );
}

/**
 * The DB-free plan pre-check: {@link validate} the migration, then project the
 * verdict + authored ops into a `PlanReport`. This is the fast pre-apply gate;
 * `dryRun` (the full shadow verification) is deferred.
 */
export function plan(opts: HostPlanOptions): PlanReport {
  const addon = loadAddon();
  const envelope = authorEnvelope(addon, opts.migration, opts.nameFallback);
  const verdict = validateEnvelope(addon, envelope, opts);
  return {
    ok: verdict.ok,
    ir_version: verdict.irVersion,
    op_count: verdict.opCount,
    error: verdict.error,
    envelope,
  };
}

/** Options for {@link status}/{@link history}. */
export interface HostStatusOptions {
  ownerApp: string;
  projectSchema: string;
  driver: DriverConfig;
  registry?: Record<string, string>;
  /** Ordered migration modules to reconcile as complete lowered plans. When
   *  omitted, `status` preserves the legacy journal-only read behavior. */
  migrations?: readonly MigrationModule[];
  /** Optional filename-derived fallback name for each migration. Must be the
   *  same length as `migrations` when supplied. */
  nameFallbacks?: readonly string[];
  /** Optional table-shape policy ceiling; use the same value as apply. */
  policyCeiling?: string;
  /** Legacy single fallback used when `nameFallbacks[index]` is absent. */
  nameFallback?: string;
}

/**
 * `status` — reconcile against the live journal over the host driver. Returns
 * the typed `StatusReply` (no JSON parse; `currentVersion` camelCase).
 *
 * When `migrations` is supplied, status reconciles the same complete ordered plans
 * used by apply. Top-level `applied`/`pending` values are logical plan ids;
 * `plans[].steps` exposes schema, data, backfill, and online-step state. Omitting
 * `migrations` returns journal-only status.
 */
export async function status(opts: HostStatusOptions): Promise<StatusReply> {
  const addon = loadAddon();
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    if (opts.migrations !== undefined) {
      if (
        opts.nameFallbacks !== undefined &&
        opts.nameFallbacks.length !== opts.migrations.length
      ) {
        throw new Error(
          "zero-migrate-cli: status nameFallbacks must match migrations length",
        );
      }
      const envelopes = opts.migrations.map((migration, index) =>
        authorEnvelope(
          addon,
          migration,
          opts.nameFallbacks?.[index] ?? opts.nameFallback,
        ),
      );
      return await addon.statusIr(hostDriver, {
        ownerApp: opts.ownerApp,
        projectSchema: opts.projectSchema,
        dialect: dialectOf(opts.driver),
        registry: opts.registry ?? {},
        envelopes,
        policyCeiling: opts.policyCeiling,
      });
    }
    return await addon.status(hostDriver, {
      projectId: opts.projectSchema,
      projectSchema: opts.projectSchema,
      dialect: dialectOf(opts.driver),
      migrations: [], // the read path / empty-journal flow (see doc).
    });
  } finally {
    await close();
  }
}

/** `history` — the journal audit trail over the host driver. Returns the
 *  typed `HistoryReply` (no JSON parse; `eventSeq` is a `bigint`). */
export async function history(opts: HostStatusOptions): Promise<HistoryReply> {
  if (opts.driver.kind !== "postgres") {
    throw new Error("zero-migrate-cli: history supports only PostgreSQL");
  }
  const addon = loadAddon();
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    return await addon.history(hostDriver, {
      projectId: opts.projectSchema,
      projectSchema: opts.projectSchema,
    });
  } finally {
    await close();
  }
}

/** Author the `{ ir_version, name, ops }` envelope from a migration module, sourcing
 *  `ir_version` from the addon (single source of truth). */
function authorEnvelope(
  addon: MigrateAddon,
  migration: MigrationModule,
  nameFallback?: string,
): IrEnvelope {
  return buildEnvelope(migration, { irVersion: addon.irVersion(), nameFallback });
}
