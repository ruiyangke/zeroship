// `zero-migrate-cli` — the creator-facing HOST runtime.
//
// A thin async layer over the prebuilt N-API addon (`crates/zero-migrate-node`).
// The creator never sees N-API, `driver::Row`, or the `hostDriver`
// callback:
//
//   import { apply, plan, status, history, validate } from "zero-migrate-cli";
//   await apply({ migration, ownerApp, projectSchema, policy, driver: { kind:"postgres", url } });
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
  type RollbackReply,
  type RollbackTargetDto,
  type StatusReply,
  type HistoryReply,
  type LoadVerifyReply,
  type PreviewSqlSource,
} from "./addon.js";
import { openPgSession } from "./driver-pg.js";
import { openMysqlSession } from "./driver-mysql2.js";
import {
  buildEnvelope,
  type IrEnvelope,
  type MigrationModule,
} from "zero-migrate/internal/recorder";

export { currentIrVersion } from "./addon.js";
export type { PreviewSqlSource } from "./addon.js";
export type { IrEnvelope, MigrationModule } from "zero-migrate/internal/recorder";

/** A driver target the facade opens a pinned host session against.
 *
 *  Both NETWORK dialects ride the SAME `SqlSession` seam: `postgres` (`pg`) and
 *  `mysql` (`mysql2`). Each dialect's lock / journal / placeholder SQL lives in
 *  its own `MigrationBackend` (`PostgresBackend` — `pg_advisory_lock`, `SET ROLE`;
 *  `MysqlBackend` — `GET_LOCK`, `?` placeholders), and the addon selects the
 *  backend from the `dialect` string. SQLite is NOT a host driver — it runs
 *  in-process via rusqlite and never crosses the seam. */
/** Host-enforced transport controls for a network driver (the host owns the
 *  socket). All optional; an absent field leaves the corresponding control off.
 *  These reach the pinned session drivers (`driver-pg.ts` / `driver-mysql2.ts`),
 *  which is why they must be carried on the driver config rather than dropped. */
export interface NetworkSecurityOptions {
  /** PEM CA bundle to PIN (TLS). When set, the server certificate is verified
   *  against it. Distinct from any `sslmode`/`ssl` in the connection URL. */
  tlsCa?: string;
  /** Reject the connection before opening a socket if the URL host is not in
   *  this allowlist. */
  hostAllowlist?: string[];
  /** Per-verb query timeout in milliseconds. */
  queryTimeoutMs?: number;
}

export type DriverConfig =
  | { kind: "postgres"; url: string; security?: NetworkSecurityOptions }
  | { kind: "mysql"; url: string; security?: NetworkSecurityOptions }
  | { kind: "sqlite"; appPath: string; journalPath: string };

type NetworkDriverConfig = Exclude<DriverConfig, { kind: "sqlite" }>;

/** Render already-authored IR envelope JSON through the addon's offline SQL
 * preview renderer. This never opens a database connection. */
export function previewSql(source: PreviewSqlSource): string[] {
  return loadAddon().previewSql(source);
}

/** The dialect string the addon lower + load-verify + backend-select expect. */
function dialectOf(driver: DriverConfig): "postgres" | "mysql" | "sqlite" {
  return driver.kind;
}

/** Open the pinned host session for a driver and return its `hostDriver` callback +
 *  a `close()`. PG uses `driver-pg.ts` (connection-scoped exact-integer parsers);
 *  MySQL uses `driver-mysql2.ts` (real `mysql2/promise`, BIGINT/DECIMAL
 *  as exact strings). Both open sessions return the SAME `AddonHostDriver` shape (the
 *  generated `JsRequest`/`JsReply`/`JsError` DTOs) — no cast. */
async function openSession(
  driver: NetworkDriverConfig,
): Promise<{ hostDriver: AddonHostDriver; close: () => Promise<void> }> {
  if (driver.kind === "postgres") {
    const s = await openPgSession(driver.url, driver.security);
    return { hostDriver: s.hostDriver, close: s.close };
  }
  if (driver.kind === "mysql") {
    const s = await openMysqlSession(driver.url, driver.security);
    return { hostDriver: s.hostDriver, close: s.close };
  }
  const unreachable: never = driver;
  throw new Error(`zero-migrate-cli: unsupported network driver ${String(unreachable)}`);
}

/** Common inputs to the host verbs. */
export interface HostApplyOptions {
  /** The migration module (an imported `.ts`/`.js` exporting `up()` or
   *  `default.up`). Resolved to an envelope by the recorder. */
  migration: MigrationModule;
  /** Ordered authored migrations that precede `migration`. Their declarations
   *  may seed logical schema contracts only when the addon proves every plan is
   *  already fully applied with its exact journal checksum. */
  priorMigrations?: readonly MigrationModule[];
  /** Optional recorder name fallbacks aligned one-for-one with
   *  `priorMigrations`. */
  priorNameFallbacks?: readonly (string | undefined)[];
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
  /** Required ordered table-shape policy documents. The first document is the
   *  trusted root/bound; each later document is an untrusted narrowing layer. */
  policy: readonly string[];
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

/** Fail fast at the JavaScript boundary too; TypeScript's required property does
 * not protect plain JavaScript callers. */
function assertExplicitPolicy(
  value: unknown,
  verb: "apply" | "rollback" | "status" | "history" | "resolvePending",
): asserts value is readonly string[] {
  if (
    !Array.isArray(value) ||
    value.length === 0 ||
    value.some((document) => typeof document !== "string" || document.length === 0)
  ) {
    throw new Error(
      `zero-migrate-cli: ${verb} requires an explicit policy (a non-empty ordered array of policy documents)`,
    );
  }
}

/**
 * Author the envelope (pure JS) then drive the addon's apply path. PostgreSQL
 * and MySQL use the host-driver `applyIr` seam; SQLite sends the complete ordered
 * envelope sequence to bundled rusqlite through `applyIrSqlite`. Resolves to the
 * typed `ApplyReply`. Network sessions are always closed (success or throw).
 */
export async function apply(opts: HostApplyOptions): Promise<ApplyOutcome> {
  assertExplicitPolicy(opts.policy, "apply");
  const addon = loadAddon();
  const priorMigrations = opts.priorMigrations ?? [];
  if (
    opts.priorNameFallbacks !== undefined &&
    opts.priorNameFallbacks.length !== priorMigrations.length
  ) {
    throw new Error(
      "zero-migrate-cli: apply priorNameFallbacks must match priorMigrations length",
    );
  }
  const priorEnvelopes = priorMigrations.map((migration, index) =>
    authorEnvelope(addon, migration, opts.priorNameFallbacks?.[index]),
  );
  const envelope = authorEnvelope(addon, opts.migration, opts.nameFallback);

  if (opts.driver.kind === "sqlite") {
    return await addon.applyIrSqlite(opts.driver.appPath, opts.driver.journalPath, {
      ownerApp: opts.ownerApp,
      projectSchema: opts.projectSchema,
      registry: opts.registry ?? {},
      charterLayers: [...opts.policy],
      approved: opts.approved ?? false,
      envelopes: [...priorEnvelopes, envelope],
    });
  }

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
      priorEnvelopes,
      envelope,
      charterLayers: [...opts.policy],
      approved: opts.approved ?? false,
      appliedBy: opts.appliedBy ?? "host",
    });
  } finally {
    await close();
  }
}

/** Options for unwinding applied migrations. */
export interface HostRollbackOptions {
  /** The complete ordered authored migration set, oldest first. A rollback
   *  reconstructs each `down` from its authored envelope, so a migration missing
   *  here has no reverse SQL and the unwind refuses rather than skipping it. */
  migrations: readonly MigrationModule[];
  /** Optional recorder name fallbacks aligned one-for-one with `migrations`. */
  nameFallbacks?: readonly (string | undefined)[];
  /** The app id that authored the migrations. It reproduces the plan identities
   *  the deploy journaled, so it must match what applied them. */
  ownerApp: string;
  /** The confined project schema the lower pins ops to. */
  projectSchema: string;
  /** The target DB driver. */
  driver: DriverConfig;
  /** The project's `{ table: owner_app }` registry. Defaults to `{}`. */
  registry?: Record<string, string>;
  /** Required ordered table-shape policy documents, identical to apply's. The
   *  guard over each `down` is composed from these. */
  policy: readonly string[];
  /** The migrator role to `SET ROLE` under. Not accepted for SQLite. */
  migratorRole?: string;
  /** How far to unwind. Required: there is no default, because every default is
   *  a guess about how much of a schema to remove. */
  target: RollbackTargetDto;
  /** Whether the teardown is approved. A `down` is destructive by construction,
   *  so the engine refuses without it. Default `false`. */
  approved?: boolean;
  /** Skip a migration that declares no `down` instead of refusing. Honored only
   *  with `backupAcknowledged`. Default `false`. */
  force?: boolean;
  /** Acknowledge that a backup exists. Default `false`. */
  backupAcknowledged?: boolean;
  /** The audit label recorded with the `rolled_back` events. Default `"host"`. */
  appliedBy?: string;
}

/** The typed `rollback` reply - re-exported from the generated addon DTOs. */
export type RollbackOutcome = RollbackReply;

/**
 * Author every envelope (pure JS) then drive the addon's rollback path.
 * PostgreSQL and MySQL go over the host-driver `rollback` seam; SQLite goes to
 * bundled rusqlite through `rollbackSqlite`. Network sessions are always closed.
 *
 * Unlike `apply`, this takes the WHOLE authored set rather than one migration and
 * its priors: the versions being unwound are already applied, so there is no
 * current envelope to distinguish, and the engine needs every candidate's `down`
 * in one set to refuse coherently.
 */
export async function rollback(opts: HostRollbackOptions): Promise<RollbackOutcome> {
  assertExplicitPolicy(opts.policy, "rollback");
  const addon = loadAddon();
  if (
    opts.nameFallbacks !== undefined &&
    opts.nameFallbacks.length !== opts.migrations.length
  ) {
    throw new Error(
      "zero-migrate-cli: rollback nameFallbacks must match migrations length",
    );
  }
  const envelopes = opts.migrations.map((migration, index) =>
    authorEnvelope(addon, migration, opts.nameFallbacks?.[index]),
  );
  const shared = {
    ownerApp: opts.ownerApp,
    projectSchema: opts.projectSchema,
    registry: opts.registry ?? {},
    envelopes,
    charterLayers: [...opts.policy],
    target: opts.target,
    approved: opts.approved ?? false,
    force: opts.force ?? false,
    backupAcknowledged: opts.backupAcknowledged ?? false,
    appliedBy: opts.appliedBy ?? "host",
  };

  if (opts.driver.kind === "sqlite") {
    return await addon.rollbackSqlite(opts.driver.appPath, opts.driver.journalPath, {
      ...shared,
      dialect: "sqlite",
    });
  }

  const { hostDriver, close } = await openSession(opts.driver);
  try {
    return await addon.rollback(hostDriver, {
      ...shared,
      migratorRole: opts.migratorRole,
      dialect: dialectOf(opts.driver),
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
  /** Required ordered table-shape policy documents, root/bound first. */
  policy: readonly string[];
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
  assertExplicitPolicy(opts.policy, "resolvePending");
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
      charterLayers: [...opts.policy],
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

/** Inputs shared by both forms of {@link status}. */
interface HostStatusBaseOptions {
  ownerApp: string;
  projectSchema: string;
  driver: DriverConfig;
  registry?: Record<string, string>;
  /** Required ordered table-shape policy documents, root/bound first. */
  policy: readonly string[];
}

/** Plan-aware status reconciles authored migrations through the same explicit
 * policy documents used by {@link apply}. */
export interface HostReconciledStatusOptions extends HostStatusBaseOptions {
  /** Ordered migration modules to reconcile as complete lowered plans. When
   *  present, `status` returns plan-aware reconciliation. */
  migrations: readonly MigrationModule[];
  /** Optional filename-derived fallback name for each migration. Must be the
   *  same length as `migrations` when supplied. */
  nameFallbacks?: readonly string[];
  /** Legacy single fallback used when `nameFallbacks[index]` is absent. */
  nameFallback?: string;
}

/** Plan-aware status for callers that already authored each migration exactly
 * once. This is used by the CLI's live plan path so the same envelopes are
 * reconciled and rendered. */
export interface HostEnvelopeStatusOptions extends HostStatusBaseOptions {
  envelopes: readonly IrEnvelope[];
  /** Reconcile without bootstrapping absent journal objects. Default `false`. */
  readOnly?: boolean;
}

/** Journal-only status does not lower authored migrations, but still requires
 * the executor's explicitly authored policy documents. */
export interface HostJournalStatusOptions extends HostStatusBaseOptions {
  migrations?: undefined;
  nameFallbacks?: never;
  nameFallback?: never;
}

/** Options for {@link status}. */
export type HostStatusOptions =
  | HostReconciledStatusOptions
  | HostJournalStatusOptions;

/** Options for {@link history}; history reads the journal through an executor
 * configured with explicitly authored policy documents. */
export interface HostHistoryOptions {
  ownerApp: string;
  projectSchema: string;
  driver: DriverConfig;
  /** Required ordered table-shape policy documents, root/bound first. */
  policy: readonly string[];
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
  if (opts.driver.kind === "sqlite") {
    throw new Error(
      "zero-migrate-cli: status is not available for SQLite; SQLite apply is supported",
    );
  }
  assertExplicitPolicy(opts.policy, "status");
  if (opts.migrations !== undefined) {
    if (
      opts.nameFallbacks !== undefined &&
      opts.nameFallbacks.length !== opts.migrations.length
    ) {
      throw new Error(
        "zero-migrate-cli: status nameFallbacks must match migrations length",
      );
    }
    const addon = loadAddon();
    const envelopes = opts.migrations.map((migration, index) =>
      authorEnvelope(
        addon,
        migration,
        opts.nameFallbacks?.[index] ?? opts.nameFallback,
      ),
    );
    return await statusEnvelopes({
      ownerApp: opts.ownerApp,
      projectSchema: opts.projectSchema,
      driver: opts.driver,
      registry: opts.registry,
      policy: opts.policy,
      envelopes,
    });
  }

  const addon = loadAddon();
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    return await addon.status(hostDriver, {
      projectId: opts.projectSchema,
      projectSchema: opts.projectSchema,
      dialect: dialectOf(opts.driver),
      migrations: [], // the read path / empty-journal flow (see doc).
      charterLayers: [...opts.policy],
    });
  } finally {
    await close();
  }
}

/** Reconcile a complete ordered set of already-authored envelopes. */
export async function statusEnvelopes(
  opts: HostEnvelopeStatusOptions,
): Promise<StatusReply> {
  if (opts.driver.kind === "sqlite" && opts.readOnly !== true) {
    throw new Error(
      "zero-migrate-cli: status is not available for SQLite; SQLite apply is supported",
    );
  }
  assertExplicitPolicy(opts.policy, "status");
  const addon = loadAddon();
  const request = {
    ownerApp: opts.ownerApp,
    projectSchema: opts.projectSchema,
    dialect: dialectOf(opts.driver),
    registry: opts.registry ?? {},
    envelopes: [...opts.envelopes],
    charterLayers: [...opts.policy],
    readOnly: opts.readOnly ?? false,
  };
  if (opts.driver.kind === "sqlite") {
    return await addon.statusIrSqlite(
      opts.driver.appPath,
      opts.driver.journalPath,
      request,
    );
  }
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    return await addon.statusIr(hostDriver, request);
  } finally {
    await close();
  }
}

/** `history` — the journal audit trail over the host driver. Returns the
 *  typed `HistoryReply` (no JSON parse; `eventSeq` is a `bigint`). */
export async function history(opts: HostHistoryOptions): Promise<HistoryReply> {
  if (opts.driver.kind !== "postgres") {
    throw new Error("zero-migrate-cli: history supports only PostgreSQL");
  }
  assertExplicitPolicy(opts.policy, "history");
  const addon = loadAddon();
  const { hostDriver, close } = await openSession(opts.driver);
  try {
    return await addon.history(hostDriver, {
      projectId: opts.projectSchema,
      projectSchema: opts.projectSchema,
      charterLayers: [...opts.policy],
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
