/**
 * Apply the committed migrations to the dev SQLite file AHEAD of the worker.
 *
 * This makes the dev tier match the platform: on Postgres the `migrated`
 * service applies the schema at deploy, before the app serves. On SQLite the
 * schema must likewise exist before the app serves — creating it lazily inside
 * the worker, one collection at a time in registration order, is backwards and
 * is the root of the "SCHEMA-INIT" class of dev-only bugs.
 *
 * PATHS - these are the exact files the worker opens, verified rather than
 * assumed:
 *
 *   DATABASE_URL           `sqlite:.zeroship/dev.sqlite`   (dev-db.ts)
 *   SqliteBackend::open    a FILE arg gives session = that file,
 *                          `db_dir = path.parent()`        (backend/sqlite/mod.rs)
 *   per-DATABASE file      `<db_dir>/zs-<alias>.sqlite`, and the alias is the
 *                          binding's schema                 (`attach_alias_file`,
 *                          `database_alias`, backend/sqlite/mod.rs)
 *   the schema             `db_<database_id>`               (`schema_name`,
 *                          crates/zeroship-core/src/database_derivation.rs)
 *   database_id in dev     `databases.<label>.id` of zeroship.jsonc, the app's
 *                          `primary` (`resolve_dev_database`,
 *                          crates/zeroship-cli/src/main.rs)
 *
 * ONE IDENTITY, and it is the declared one: the apply and the runtime compose
 * the same name because they read the same `dbs_` id out of the same file.
 * Getting it wrong is the failure worth guarding against: the apply would report
 * `applied: [...]` against a file nobody opens, and the app would still be broken
 * with a success line in the log.
 *
 * The app id is a different fact and keeps its own job here: it stamps
 * `owner_app` provenance on every applied migration and owns each collection in
 * the registry. It names no file.
 */
import { mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { sqliteDevFilePath } from "../dev-database-url.js";

import { loadMigrateAddon, type ApplyReply } from "./addon.js";
import { CONFINED_SYSTEM_SHAPE_INJECT_TOML } from "./confined-system-shape.generated.js";
import { recordMigrationsDir } from "./recorder.js";

/** The fixed AppId shared by local development hosts. */
export const DEV_APP_ID = "app_0000000002e4nenowz3qmamtd";

/** `.zeroship`, the directory the dev DATABASE_URL's parent resolves to. */
export const DEV_STATE_DIR = ".zeroship";

/**
 * The confined charter the dev apply runs under.
 *
 * The GRANTS are this path's own; the `[[inject]]` block is the platform-wide
 * fragment `policies/confined-system-shape.inject.toml`, the same bytes the
 * emit ceiling in `./confined-ceiling.ts` and the deployed server ceiling take.
 * That the three describe the SAME table is load-bearing: if they disagreed,
 * gen-types' descriptor and the applied schema would disagree, and the engine's
 * own contract is that emit and apply are byte-identical. There is one copy of
 * the rule and everything concatenates it.
 *
 * The grants are what the emit path does NOT need, because emit renders no DDL:
 * creating tables, renaming, and destructive ops. Deliberately ABSENT are
 * `runtime.lock_timeout_ms` / `runtime.statement_timeout_ms` — the vendored
 * engine turned those into declared-only knobs that REJECT a non-default value
 * (`DeclaredOnlyNonDefault`), so including them fails the apply outright.
 */
export const CONFINED_APPLY_CHARTER_TOML =
  `policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "schema.rename"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

` + CONFINED_SYSTEM_SHAPE_INJECT_TOML;

/**
 * The state directory the worker's SQLite backend will use, derived from the
 * SAME `DATABASE_URL` the runtime is spawned with.
 *
 * This must NOT be hardcoded to `<root>/.zeroship`. `DATABASE_URL` is
 * overridable (shell, then `.env`, then the dev default), and applying to a
 * different file than the worker opens is a silent failure: the apply reports
 * `applied: [...]`, the boot log looks healthy, and every data call still fails
 * with `no such table`.
 *
 * Mirrors `SqliteBackend::open`: a `sqlite:` URL naming a FILE makes that file
 * the session, and `db_dir` its parent. A relative path is relative to `root`,
 * which is the dev server's cwd and therefore the runtime's.
 */
export function devSqliteDir(root: string, databaseUrl?: string): string {
  if (databaseUrl === undefined) return join(root, DEV_STATE_DIR);
  return dirname(resolve(root, sqliteDevFilePath(databaseUrl)));
}

/**
 * The ATTACH alias one database is addressable under, which is its schema.
 *
 * Mirrors `schema_name` in `crates/zeroship-core/src/database_derivation.rs`.
 * The SQLite backend names the file after the alias, so this is the only place
 * on this side that turns a `dbs_` id into a file name.
 */
export function devDatabaseAlias(databaseId: string): string {
  return `db_${databaseId}`;
}

/**
 * The database file + migration journal for one declared database.
 *
 * `databaseId` is REQUIRED and has no default. The runtime opens the file the
 * BINDING names, and the binding names the database the project declared; a
 * default here would be this side inventing an identity for a fact the file
 * already states, which is how the apply and the runtime came to fill two
 * different files.
 */
export function devSqlitePaths(
  root: string,
  databaseId: string,
  databaseUrl?: string,
): {
  appPath: string;
  journalPath: string;
} {
  const dir = devSqliteDir(root, databaseUrl);
  const alias = devDatabaseAlias(databaseId);
  return {
    appPath: join(dir, `zs-${alias}.sqlite`),
    journalPath: join(dir, `zs-${alias}.migrations.sqlite`),
  };
}

/**
 * Record `migrationsDir` and apply every pending envelope to the dev SQLite
 * app file, in authored order, through the addon's in-process verb.
 *
 * Idempotent: the engine's `_mig` journal skips migrations it has already
 * applied, so this is safe to run on every dev boot and on migration
 * hot-update.
 *
 * Returns the engine's reply so the caller can log what actually happened
 * rather than "done" — `applied` being empty on a fresh database is a signal,
 * not a success.
 */
export async function applyMigrationsToDevSqlite(opts: {
  root: string;
  migrationsDir: string;
  /** Collection names from the generated descriptor; every one maps to the dev app. */
  collections: string[];
  /**
   * The `dbs_` id `databases.<label>.id` declares for the database being
   * migrated. It names the FILE, and the dev runtime attaches the same one.
   */
  databaseId: string;
  appId?: string;
  /**
   * The resolved `DATABASE_URL` the runtime will be spawned with. Pass the
   * value from `resolveDatabaseUrl`, never a re-derived one: the apply and the
   * worker must agree on the file, and the only way to guarantee that is to
   * share the resolution rather than repeat it.
   */
  databaseUrl?: string;
}): Promise<ApplyReply> {
  const appId = opts.appId ?? DEV_APP_ID;
  const { appPath, journalPath } = devSqlitePaths(
    opts.root,
    opts.databaseId,
    opts.databaseUrl,
  );

  // The apply runs BEFORE the runtime spawns, and `.zeroship/` is normally
  // created by the runtime — so on a cold checkout (or after `rm -rf
  // .zeroship`) the directory does not exist yet and SQLite cannot create a
  // file inside it. Owning the directory here keeps the ordering one-way: the
  // apply depends on nothing the runtime has done, which is the whole point of
  // applying ahead.
  await mkdir(dirname(appPath), { recursive: true });

  const envelopes = await recordMigrationsDir(opts.migrationsDir);

  const registry: Record<string, string> = {};
  for (const name of opts.collections) registry[name] = appId;

  return loadMigrateAddon().applyIr(null, {
    ownerApp: appId,
    // SQLite renders unqualified `main` and is schema-inert, but the value
    // still flows through lowering and executor confinement, so it must match
    // what gen-types emitted ("public").
    projectSchema: "public",
    dialect: "sqlite",
    // The addon opens both files itself, so there is no host driver and no
    // network session. The dialect above is a separate fact and stays separate:
    // this names the transport, not the vendor.
    driver: { kind: "inProcess", appPath, journalPath },
    registry,
    charterLayers: [CONFINED_APPLY_CHARTER_TOML],
    // The operator owns the local dev file; there is no approval workflow to
    // gate a developer's own machine behind.
    approved: true,
    envelopes: envelopes as unknown[] as never,
  });
}
