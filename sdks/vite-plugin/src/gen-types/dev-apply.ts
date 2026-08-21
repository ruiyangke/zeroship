/**
 * Apply the committed migrations to the dev SQLite file AHEAD of the worker.
 *
 * This makes the dev tier match the platform: on Postgres the `migrated`
 * service applies the schema at deploy, before the app serves, and
 * `registerModel` is a pure no-op. On SQLite the worker used to create the
 * schema itself, lazily, one collection at a time, in registration order —
 * which is backwards, and is the root of the "SCHEMA-INIT" class of dev-only
 * bugs. See
 * docs/proposals/2026-08-09-dev-sqlite-migration-apply-ahead-of-runtime.md.
 *
 * PATHS — these are the exact files the worker opens, verified rather than
 * assumed:
 *
 *   DATABASE_URL           `sqlite:.zeroship/dev.sqlite`   (dev-db.ts)
 *   SqliteBackend::open    a FILE arg ⇒ session = that file,
 *                          `db_dir = path.parent()`        (backend/sqlite/mod.rs)
 *   per-app file           `<db_dir>/zs-<app_id>.sqlite`   (backend/sqlite/mod.rs)
 *   app_id in dev          `env_vars["APP_ID"]`, else the literal `"default"`
 *                          (crates/runtime/src/core/plugin.rs)
 *
 * so `.zeroship/zs-default.sqlite` + `.zeroship/zs-default.migrations.sqlite`.
 * Getting `app_id` wrong is the failure worth guarding against: `applyIrSqlite`
 * would report `applied: [...]` against a file nobody opens, and the app would
 * still be broken with a success line in the log.
 */
import { mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";

import { loadMigrateAddon, type ApplyReply } from "./addon.js";
import { recordMigrationsDir } from "./recorder.js";

/** The dev app_id, mirroring `crates/runtime/src/core/plugin.rs`'s fallback. */
export const DEV_APP_ID = "default";

/** `.zeroship`, the directory the dev DATABASE_URL's parent resolves to. */
export const DEV_STATE_DIR = ".zeroship";

/**
 * The confined charter the dev apply runs under.
 *
 * The `[[inject]]` block is copied VERBATIM from
 * `./confined-ceiling.ts`'s `CONFINED_SCHEMA_EMIT_CEILING_TOML`. That the two
 * describe the SAME table is load-bearing: if they disagree, gen-types'
 * descriptor and the applied schema disagree, and the engine's own contract is
 * that emit and apply are byte-identical.
 *
 * The `NOW()` SPELLING is not. This comment claimed it was until 2026-08-20.
 * `inject_default_to_ir` lower-cases the token before matching it against
 * `"now" | "now()" | "current_timestamp"`
 * (third_party/zero-migrate/crates/zero-migrate/src/model/table_shape.rs), so
 * all three spellings lower to the identical `SynthFn::Now`. `NOW()` is used
 * here because every other copy uses it and
 * `tests/inject_policy_mirror_gate.sh` compares the copies byte for byte after
 * whitespace folding - a cosmetic difference it cannot tell from a real one is
 * worth removing, which is a smaller claim than the one this comment made.
 *
 * The grants are what the emit path does NOT need, because emit renders no DDL:
 * creating tables, renaming, and destructive ops. Deliberately ABSENT are
 * `runtime.lock_timeout_ms` / `runtime.statement_timeout_ms` — the vendored
 * engine turned those into declared-only knobs that REJECT a non-default value
 * (`DeclaredOnlyNonDefault`), so including them fails the apply outright.
 */
export const CONFINED_APPLY_CHARTER_TOML = `policy_version = 1

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

# The mandatory platform system-table shape — the seven system columns + the
# ["id"] primary key + the three system indexes injected into every created
# table. MUST stay identical to the [[inject]] rule in confined-ceiling.ts.
[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  # The three NOT NULL columns carry defaults because the data plane does not
  # send them. crud/system_fields_pass.rs omits created_at/updated_at/version
  # from every INSERT by design and relies on the DDL to supply the canonical
  # value; without these the first insert into any migration-created table fails
  # with "null value in column created_at violates not-null constraint".
  # id is deliberately defaultless - it is minted into the INSERT instead.
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false, default = "NOW()" },
  { name = "updated_at", type = "timestamptz", nullable = false, default = "NOW()" },
  { name = "created_by", type = "text",        nullable = true  },
  { name = "updated_by", type = "text",        nullable = true  },
  { name = "version",    type = "integer",     nullable = false, default = "1" },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [
  { name = "ix_deleted_at", columns = ["deleted_at"] },
  { name = "ix_updated_at", columns = ["updated_at"] },
  { name = "ix_created_by", columns = ["created_by"] },
]
`;

/**
 * The state directory the worker's SQLite backend will use, derived from the
 * SAME `DATABASE_URL` the runtime is spawned with.
 *
 * This must NOT be hardcoded to `<root>/.zeroship`. `DATABASE_URL` is
 * overridable (shell, then `.env`, then the dev default), and applying to a
 * different file than the worker opens is a silent failure: the apply reports
 * `applied: [...]`, the boot log looks healthy, and every data call still fails
 * with `no such table`. That is exactly what happened to
 * `tests/e2e-browser`, which gives each demo a private state dir via
 * `DATABASE_URL=sqlite:<stateDir>/dev.sqlite` so two runs cannot collide -- the
 * apply wrote to `examples/db-todos/.zeroship` while the worker read
 * `<stateDir>`, and the suite failed with
 *
 *     db: no such table: default.users
 *
 * Mirrors `SqliteBackend::open`: a `sqlite:` URL naming a FILE makes that file
 * the session, and `db_dir` its parent. A relative path is relative to `root`,
 * which is the dev server's cwd and therefore the runtime's.
 */
export function devSqliteDir(root: string, databaseUrl?: string): string {
  const url = databaseUrl ?? "";
  if (url.startsWith("sqlite:")) {
    const path = url.slice("sqlite:".length);
    // `:memory:` (and the `sqlite::memory:` spelling) name no file on disk, so
    // there is nothing to apply into; fall back rather than compute a
    // nonsensical parent directory.
    if (path.length > 0 && !path.startsWith(":memory:")) {
      return dirname(resolve(root, path));
    }
  }
  return join(root, DEV_STATE_DIR);
}

/** The app file + journal the worker's SQLite backend derives for dev. */
export function devSqlitePaths(
  root: string,
  appId: string = DEV_APP_ID,
  databaseUrl?: string,
): {
  appPath: string;
  journalPath: string;
} {
  const dir = devSqliteDir(root, databaseUrl);
  return {
    appPath: join(dir, `zs-${appId}.sqlite`),
    journalPath: join(dir, `zs-${appId}.migrations.sqlite`),
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
  const { appPath, journalPath } = devSqlitePaths(opts.root, appId, opts.databaseUrl);

  // The apply runs BEFORE the runtime spawns, and `.zeroship/` is normally
  // created by the runtime — so on a cold checkout (or after `rm -rf
  // .zeroship`) the directory does not exist yet and SQLite cannot create a
  // file inside it. Measured, not hypothesised:
  //
  //   dev migration apply FAILED: open main (app file):
  //   unable to open database file: …/.zeroship/zs-default.sqlite
  //
  // Owning the directory here keeps the ordering one-way: the apply depends on
  // nothing the runtime has done, which is the whole point of applying ahead.
  await mkdir(dirname(appPath), { recursive: true });

  const envelopes = await recordMigrationsDir(opts.migrationsDir);

  const registry: Record<string, string> = {};
  for (const name of opts.collections) registry[name] = appId;

  return loadMigrateAddon().applyIrSqlite(appPath, journalPath, {
    ownerApp: appId,
    // SQLite renders unqualified `main` and is schema-inert, but the value
    // still flows through lowering and executor confinement, so it must match
    // what gen-types emitted ("public").
    projectSchema: "public",
    registry,
    charterLayers: [CONFINED_APPLY_CHARTER_TOML],
    // The operator owns the local dev file; there is no approval workflow to
    // gate a developer's own machine behind.
    approved: true,
    envelopes: envelopes as unknown[] as never,
  });
}
