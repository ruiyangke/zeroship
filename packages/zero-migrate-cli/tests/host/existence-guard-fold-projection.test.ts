// The pending-schema projection versus an existence guard's SatisfiedNoop verdict.
//
// `apply()` takes one of two lowering paths depending ONLY on whether
// `priorMigrations` is empty. With no priors the guard is evaluated at apply time
// against the live catalog. With priors, lowering first PROJECTS the pending ops
// onto the live snapshot, and that fold is guard-blind: a guarded
// `createTable ifNotExists` over a table that already exists folds to
// "table already exists" and the whole plan is refused -- pre-empting the guard
// whose entire job is to report the object as already present.
//
// Ten arms, because "the projection honours the guard" and "the projection stopped
// detecting duplicates" look identical from one:
//   - adopt: live table matches the declaration EXACTLY and the incoming registry
//     names this app as the owner -- MUST apply cleanly;
//   - divergent: the same guarded op over a live table whose column width differs --
//     MUST still refuse, and the refusal must name the DIVERGENCE, not the name
//     collision, because a projection that skipped on the fold's error kind would
//     skip on NAME presence alone and journal a completed row over a wrong shape;
//   - branch witness: the one shape BOTH lowering paths refuse, an unguarded duplicate
//     create, driven down each -- the refusals MUST come from different layers, or the
//     parity arm below is comparing one path with itself;
//   - parity: the same guarded op with the table ABSENT from the incoming registry --
//     which is the DEFAULT state, since `--registry` is optional and `loadRegistry`
//     returns `{}` -- MUST adopt on BOTH lowering paths. Empty priors skip the
//     projection and adopt (the contract `existence-guard-varchar-adoption.test.ts`
//     pins); non-empty priors must reach the same verdict, so that migration COUNT
//     cannot decide whether an authored adoption is legal;
//   - foreign owner: the same guarded op with the registry EXPLICITLY naming another
//     app -- MUST refuse, and the refusal must be the loader's `ownership violation`,
//     since that is the layer that makes the distinction the projection cannot;
//   - unguarded control: the same createTable WITHOUT `ifNotExists` -- MUST still
//     refuse, which isolates the guard as what changed rather than duplicate
//     detection in general;
//   - MySQL: the guarded op MUST refuse exactly as it does today. MySQL evaluates
//     no existence probe at apply time, so letting a SatisfiedNoop through the
//     projection would send bare DDL to a backend that cannot check anything;
//   - SQLite adopt: plan and apply MUST both accept the matching live table;
//   - SQLite divergent: plan and apply MUST both refuse, both naming the divergence;
//   - SQLite unguarded control: plan MUST still refuse a bare duplicate create, which
//     isolates the guard as what the SQLite plan path honours.
//
// WHICH MUTATIONS THESE ARMS ACTUALLY CATCH -- measured, not claimed, against ONE
// compiled addon with the projection's ownership check re-introduced behind an env gate
// in four forms, plus one mutation of the loader:
//   - refuse whenever dropping the op would CHANGE the registry (the shipped-then-
//     reverted form): ONLY the parity arm turns red;
//   - refuse whenever dropping the op would NOT change the registry (inverted): the two
//     ADOPT arms turn red, PostgreSQL and SQLite, and the parity arm stays GREEN;
//   - refuse only when the registry EXPLICITLY names another app (narrowed): NOTHING
//     turns red -- which is the measurement that no input can select that branch;
//   - no check at all (what ships): every arm green;
//   - `enforce_ir_ownership` returning Ok unconditionally (the loader gate removed):
//     ONLY the foreign-owner arm turns red.
// So the parity arm is NOT a general detector of changes to ownership handling. It
// catches the refuse-on-any-registry-change form and nothing else; the adopt arms carry
// the inverted form; the foreign-owner arm pins the LOADER, which is the layer that
// actually decides ownership. The branch witness and both unguarded controls do not move
// under any of the five, which is what makes them controls.
//
// Every arm drives the REAL path: authored through the public `zero-migrate` API,
// lowered by the native addon, applied through `zero-migrate-cli`'s `apply()` over the
// real `pg`/`mysql2`/`sqlite` driver seams against a live database, and with the
// pre-existing table created OUT OF BAND. Seeding it through the engine would put it
// in the migration HISTORY as well as the catalog and would measure a different
// question. Every arm carries a non-empty `priorMigrations`, so the folding path is the
// one under test, EXCEPT the two that deliberately run both halves: the branch witness
// and the parity arm.
//
// SQLite is covered by the last three arms below. It reaches the projection from a
// DIFFERENT direction than PostgreSQL and MySQL: `apply()` routes a `sqlite` driver
// to `applyIrSqlite`, which drives the engine's own `deploy_envelopes` and never
// projects, while `plan`/`status` route to `statusIrSqlite`, which lowers through
// `lower_ordered_envelopes_to_plans` and does. Those arms therefore assert plan and
// apply AGREE, which is the property a split path can silently lose.
//
// Does NOT cover guarded ops other than `createTable`. That gap grew when the
// projection's ownership check was removed: a guarded `dropTable`/`renameTable` whose
// guard is satisfied also used to be refused by it, for a registry change that was
// never about foreign ownership, and no arm here drives one either way. What DOES
// cover part of it is the projection's own shape: `projection_guard_verdict`
// (`crates/zero-migrate-node/src/lower.rs`) runs `decide` over every step's probe with
// no per-op branch, so the `createTable` arms below exercise the identical code path
// for any op. What NOTHING covers is that a guarded `dropTable`/`renameTable` LOWERS a
// probe the projection can satisfy at all - that residue is a hole.
//
// Does NOT cover a partially matching table whose secondary index is still absent.
// NOTHING ELSE COVERS IT: the per-unit rule is designed for exactly that shape and no
// arm in this workspace drives one. A hole, noted rather than silently narrowed.

import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

import {
  apply,
  plan as authorOffline,
  statusEnvelopes,
  type DriverConfig,
  type MigrationModule,
} from "zero-migrate-cli";
import { table, t, type ColumnDef } from "zero-migrate";
import { noInjectPolicy } from "./policy.js";


// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_guard_fold_projection";
const OTHER_APP = "app_guard_fold_other";
const BASE_TABLE = "guard_fold_base";
const TABLE = "notes";

type NamedMigration = MigrationModule & { readonly name: string };

function authoredMigration(name: string, schema: () => void): NamedMigration {
  return { name, default: { schema } } as NamedMigration;
}

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

function mysqlIdent(value: string): string {
  return `\`${value.replaceAll("`", "``")}\``;
}

/** The prior migration. Its only job is to be an APPLIED prefix, so that the second
 *  apply carries a non-empty `priorMigrations` and takes the folding path. It does
 *  not touch `notes`. */
function baseMigration(): NamedMigration {
  return authoredMigration("guard_fold_base", () => {
    table(BASE_TABLE).create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
  });
}

/** The migration under test: `notes(id, body)` created with or without the guard.
 *  `body` is typed by the caller so the divergent arm differs in exactly one way. */
function createNotes(
  name: string,
  options: { readonly guarded: boolean; readonly body: () => ColumnDef },
): NamedMigration {
  return authoredMigration(name, () => {
    table(TABLE).create({
      columns: { id: t.int().notNull(), body: options.body() },
      primaryKey: ["id"],
      ...(options.guarded ? { ifNotExists: true as const } : {}),
    });
  });
}

async function applyAfterBase(
  migration: NamedMigration,
  projectSchema: string,
  driver: DriverConfig,
  registry: Record<string, string>,
) {
  const base = baseMigration();
  return apply({
    migration,
    priorMigrations: [base],
    priorNameFallbacks: [base.name],
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry,
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "guard-fold-projection-e2e",
    nameFallback: migration.name,
  });
}

/** The other half of the parity arm: the SAME authored migration applied with NO
 *  priors, which is the branch `verbs.rs` takes on an empty `priorMigrations` and the
 *  branch that never reaches the pending-schema projection. */
async function applyGuardedWithoutPriors(
  migration: NamedMigration,
  projectSchema: string,
  driver: DriverConfig,
) {
  return apply({
    migration,
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: {},
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "guard-fold-projection-e2e",
    nameFallback: migration.name,
  });
}

/** The `(data_type, character_maximum_length)` the live catalog holds for a column. */
async function pgColumnType(
  client: import("pg").Client,
  schema: string,
  column: string,
): Promise<{ data_type: string; character_maximum_length: number | null } | undefined> {
  const result = await client.query(
    `SELECT data_type, character_maximum_length
       FROM information_schema.columns
      WHERE table_schema = $1 AND table_name = $2 AND column_name = $3`,
    [schema, TABLE, column],
  );
  return result.rows[0] as
    | { data_type: string; character_maximum_length: number | null }
    | undefined;
}

/** The versions the project's journal reports as completed. */
async function pgCompletedVersions(
  client: import("pg").Client,
  metaSchema: string,
): Promise<string[]> {
  const result = await client.query(
    `SELECT version FROM ${pgIdent(metaSchema)}.schema_migrations ORDER BY version`,
  );
  return (result.rows as Array<{ version: string }>).map((row) => row.version);
}

/**
 * Create the schema, apply the base migration so a prefix exists, then seed `notes`
 * OUT OF BAND with `body` of the given SQL type and run the body. The out-of-band
 * CREATE is what makes the table live-only: it is in the catalog and NOT in the
 * migration history, which is the state a guard exists to absorb.
 */
async function withAppliedBaseAndSeededTable(
  prefix: string,
  bodySqlType: string,
  body: (
    client: import("pg").Client,
    schema: string,
    driver: DriverConfig,
    metaSchema: string,
  ) => Promise<void>,
): Promise<void> {
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueNamespace(prefix);
  const meta = `${schema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: PG_URL! };
  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    const base = baseMigration();
    await apply({
      migration: base,
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver,
      registry: {},
      policy: [noInjectPolicy(schema)],
      approved: true,
      appliedBy: "guard-fold-projection-e2e",
      nameFallback: base.name,
    });
    await client.query(
      `CREATE TABLE ${pgIdent(schema)}.${pgIdent(TABLE)} (
         id integer PRIMARY KEY NOT NULL,
         body ${bodySqlType}
       )`,
    );
    await body(client, schema, driver, meta);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(meta)} CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
}

/**
 * The same seeding, WITHOUT the base migration: no prefix is applied, so the migration
 * under test carries empty `priorMigrations` and takes the branch that skips the
 * projection entirely. `varchar(255)` is fixed here because the parity arm is the only
 * caller and it declares `t.string()`.
 */
async function withSeededTableOnly(
  prefix: string,
  body: (
    client: import("pg").Client,
    schema: string,
    driver: DriverConfig,
    metaSchema: string,
  ) => Promise<void>,
): Promise<void> {
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueNamespace(prefix);
  const meta = `${schema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: PG_URL! };
  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await client.query(
      `CREATE TABLE ${pgIdent(schema)}.${pgIdent(TABLE)} (
         id integer PRIMARY KEY NOT NULL,
         body varchar(255)
       )`,
    );
    await body(client, schema, driver, meta);
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(schema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(meta)} CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
}

test("PostgreSQL: a guarded createTable over a matching live table survives the pending-schema projection", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; guarded projection e2e skipped");
    return;
  }
  await withAppliedBaseAndSeededTable(
    "guardfold_adopt_pg",
    "varchar(255)",
    async (client, schema, driver, meta) => {
      // `t.string()` defaults to `length: 255`, so this declares exactly the live shape.
      const guarded = createNotes("guard_fold_adopt", { guarded: true, body: () => t.string() });
      await applyAfterBase(guarded, schema, driver, {
        [BASE_TABLE]: OWNER_APP,
        [TABLE]: OWNER_APP,
      });

      assert.deepEqual(
        await pgColumnType(client, schema, "body"),
        { data_type: "character varying", character_maximum_length: 255 },
        "the live table is untouched: the guard proved equality and the CREATE was skipped",
      );
      const completed = await pgCompletedVersions(client, meta);
      assert.equal(
        completed.length,
        2,
        `both the base and the guarded migration are journaled completed (got ${JSON.stringify(completed)})`,
      );
    },
  );
});

test("PostgreSQL: the same guarded createTable still refuses a divergent live table, naming the divergence", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; guarded projection divergence e2e skipped");
    return;
  }
  await withAppliedBaseAndSeededTable(
    "guardfold_drift_pg",
    "varchar(100)",
    async (client, schema, driver) => {
      // Live varchar(100), declared varchar(255). The ONLY difference from the arm
      // above is the live width. A projection that skipped on the fold's error kind
      // would skip here too -- the fold reports NAME presence, not shape -- and would
      // journal a completed row over a column that truncates at a different width.
      const guarded = createNotes("guard_fold_drift", { guarded: true, body: () => t.string() });
      await assert.rejects(
        applyAfterBase(guarded, schema, driver, {
          [BASE_TABLE]: OWNER_APP,
          [TABLE]: OWNER_APP,
        }),
        // Requiring BOTH widths is the point: `already exists` would also be a
        // refusal, but it is the guard-blind one this ticket is about.
        /existence-guard drift[\s\S]*column notes\.body[\s\S]*character varying\(255\)[\s\S]*character varying\(100\)/,
        "a live table whose shape diverges must be refused, and the refusal must say so",
      );

      assert.deepEqual(
        await pgColumnType(client, schema, "body"),
        { data_type: "character varying", character_maximum_length: 100 },
        "the refusal changed nothing: the live column keeps its own width",
      );
    },
  );
});

/** The message of the refusal `run` produces, or a failure if it did not refuse. */
async function refusalOf(run: () => Promise<unknown>): Promise<string> {
  try {
    await run();
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
  assert.fail("expected a refusal, got a clean apply");
}

test("PostgreSQL: the parity arm's two halves take DIFFERENT lowering branches", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; lowering-branch witness skipped");
    return;
  }
  // The witness for the arm below. A parity assertion is worth nothing if both halves
  // happen to run the same code: `apply()` forks on `prior_envelope_json.is_empty()`
  // alone (`verbs.rs`), and only the non-empty side builds a pending-schema projection.
  // Rather than assert that fork in a comment, drive the ONE shape both branches refuse
  // -- an UNGUARDED duplicate create -- down each half and require the refusals to come
  // from different layers. Only the projection can say "failed to project pending
  // schema"; the empty-priors half never builds one.
  const bare = createNotes("guard_fold_witness", { guarded: false, body: () => t.string() });

  let withoutPriors = "";
  await withSeededTableOnly("guardfold_witness_nopriors_pg", async (_client, schema, driver) => {
    withoutPriors = await refusalOf(() => applyGuardedWithoutPriors(bare, schema, driver));
  });

  let withPriors = "";
  await withAppliedBaseAndSeededTable(
    "guardfold_witness_priors_pg",
    "varchar(255)",
    async (_client, schema, driver) => {
      withPriors = await refusalOf(() => applyAfterBase(bare, schema, driver, {}));
    },
  );

  assert.match(
    withPriors,
    /failed to project pending schema/,
    `the priors half must reach the projection (got ${JSON.stringify(withPriors)})`,
  );
  assert.doesNotMatch(
    withoutPriors,
    /failed to project pending schema/,
    `the empty-priors half must NOT reach the projection (got ${JSON.stringify(withoutPriors)})`,
  );
});

test("PostgreSQL: a guarded createTable over an unregistered live table adopts identically with and without priors", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; guarded projection parity e2e skipped");
    return;
  }
  // One authored migration, one registry (`{}` -- the default state for every user who
  // never passes `--registry`), one live table seeded out of band, run down BOTH lowering
  // paths. The empty-priors path is the tested contract of
  // `existence-guard-varchar-adoption.test.ts`; this arm requires the priors path to
  // agree with it. A refusal on either half is the defect, and asserting only one half
  // is how the two paths drifted apart in the first place.
  //
  // That the two halves really are two paths is not assumed here: the arm above measures
  // it, and it is the reason this arm is not self-satisfying.
  const guarded = createNotes("guard_fold_parity", { guarded: true, body: () => t.string() });

  await withSeededTableOnly("guardfold_parity_nopriors_pg", async (client, schema, driver) => {
    await applyGuardedWithoutPriors(guarded, schema, driver);
    assert.deepEqual(
      await pgColumnType(client, schema, "body"),
      { data_type: "character varying", character_maximum_length: 255 },
      "empty priors: the guard proved equality, the CREATE was skipped, the table is untouched",
    );
  });

  await withAppliedBaseAndSeededTable(
    "guardfold_parity_priors_pg",
    "varchar(255)",
    async (client, schema, driver, meta) => {
      await applyAfterBase(guarded, schema, driver, {});
      assert.deepEqual(
        await pgColumnType(client, schema, "body"),
        { data_type: "character varying", character_maximum_length: 255 },
        "non-empty priors: the same authored op reaches the same verdict on the same shape",
      );
      const completed = await pgCompletedVersions(client, meta);
      assert.equal(
        completed.length,
        2,
        `both migrations are journaled completed, as on the empty-priors path (got ${JSON.stringify(completed)})`,
      );
    },
  );
});

test("PostgreSQL: a guarded createTable is still refused when the registry names ANOTHER app as owner", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; guarded projection foreign-owner e2e skipped");
    return;
  }
  await withAppliedBaseAndSeededTable(
    "guardfold_foreign_pg",
    "varchar(255)",
    async (client, schema, driver, meta) => {
      // Identical to the parity arm except that the registry EXPLICITLY assigns `notes`
      // to another app. Requiring the loader's wording rather than a bare /ownership/ is
      // the point: both the loader's refusal and the projection's carried that word, so
      // a loose match could not tell which layer refused -- and the projection's layer
      // is never reached, because `enforce_ir_ownership` rejects the op first. Measured:
      // disabling that loader gate is the only one of the five mutations run against this
      // file that turns THIS arm red.
      const guarded = createNotes("guard_fold_foreign", { guarded: true, body: () => t.string() });
      await assert.rejects(
        applyAfterBase(guarded, schema, driver, {
          [BASE_TABLE]: OWNER_APP,
          [TABLE]: OTHER_APP,
        }),
        new RegExp(
          `ownership violation[\\s\\S]*"${TABLE}"[\\s\\S]*owned by ${OTHER_APP}[\\s\\S]*"${OWNER_APP}"`,
        ),
        "a guarded create over a table the registry gives to another app must refuse, not adopt",
      );

      const completed = await pgCompletedVersions(client, meta);
      assert.equal(
        completed.length,
        1,
        `only the base migration is journaled; the foreign-owned adoption left no completed row (got ${JSON.stringify(completed)})`,
      );
    },
  );
});

test("PostgreSQL control: an UNGUARDED createTable over an existing live table is still refused", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; unguarded projection control skipped");
    return;
  }
  await withAppliedBaseAndSeededTable(
    "guardfold_bare_pg",
    "varchar(255)",
    async (client, schema, driver, meta) => {
      // The ONLY difference from the adopting arm is `ifNotExists`. Duplicate
      // detection is not what changed; the guard is.
      const bare = createNotes("guard_fold_bare", { guarded: false, body: () => t.string() });
      await assert.rejects(
        applyAfterBase(bare, schema, driver, {
          [BASE_TABLE]: OWNER_APP,
          [TABLE]: OWNER_APP,
        }),
        /already exists/,
        "an unguarded duplicate create must still be refused",
      );

      const completed = await pgCompletedVersions(client, meta);
      assert.equal(
        completed.length,
        1,
        `only the base migration is journaled (got ${JSON.stringify(completed)})`,
      );
    },
  );
});

test("MySQL: a guarded createTable over an existing live table is refused by the projection, unchanged", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL projection scope guard skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueNamespace("guardfold_my");
  const meta = `${database}_migrations`;
  const driver: DriverConfig = { kind: "mysql", url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    const base = baseMigration();
    await apply({
      migration: base,
      ownerApp: OWNER_APP,
      projectSchema: database,
      driver,
      registry: {},
      policy: [noInjectPolicy(database)],
      approved: true,
      appliedBy: "guard-fold-projection-e2e",
      nameFallback: base.name,
    });
    await admin.query(
      `CREATE TABLE ${mysqlIdent(database)}.${mysqlIdent(TABLE)} (
         id int NOT NULL,
         body varchar(255),
         PRIMARY KEY (id)
       )`,
    );

    // MySQL runs no existence probe at apply time, so a SatisfiedNoop let through the
    // projection would send bare DDL to a backend that cannot check anything. This arm
    // pins the refusal that is deliberately left in place.
    const guarded = createNotes("guard_fold_mysql", { guarded: true, body: () => t.string() });
    await assert.rejects(
      applyAfterBase(guarded, database, driver, {
        [BASE_TABLE]: OWNER_APP,
        [TABLE]: OWNER_APP,
      }),
      /failed to project pending schema[\s\S]*already exists/,
      "the MySQL leg keeps refusing at the projection",
    );

    const [rows] = await admin.query(
      `SELECT version FROM ${mysqlIdent(meta)}.schema_migrations ORDER BY version`,
    );
    assert.equal(
      (rows as unknown[]).length,
      1,
      "only the base migration is journaled; the refused one left no completed row",
    );
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS ${mysqlIdent(database)};
         DROP DATABASE IF EXISTS ${mysqlIdent(meta)}`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});

// -- SQLite: plan and apply reach the guard down two different code paths --------
//
// `apply()` with a `sqlite` driver calls `applyIrSqlite`, which hands the envelope
// sequence to the engine's `deploy_envelopes`. That loop re-snapshots the live
// catalog after every envelope, so it never builds a pending-schema projection and
// the guard is decided only by the backend's own `existence_probe::decide`.
// `plan`/`status` with the same driver call `statusIrSqlite`, which cannot apply
// anything and therefore SIMULATES the same sequence through
// `lower_ordered_envelopes_to_plans` -- the function that owns the projection.
//
// One authored set, one database, two verbs, opposite halves of the codebase. These
// arms exist to keep the two answers equal.

const SQLITE_PROJECT = "public";

/** The journal filename the CLI derives next to the app database. */
function sqliteDriverAt(directory: string): DriverConfig {
  return {
    kind: "sqlite",
    appPath: join(directory, "app.db"),
    journalPath: join(directory, "app.migrations.db"),
  };
}

/**
 * Author the ordered envelopes `statusEnvelopes` reconciles.
 *
 * The host package exports no `authorEnvelope`, but the DB-free `plan()` pre-check
 * returns the envelope it authored, which is the same document `apply()` builds
 * internally. Using it keeps the plan half of these arms on the public API.
 */
function sqliteEnvelopes(migrations: readonly NamedMigration[]) {
  return migrations.map(
    (migration) =>
      authorOffline({
        migration,
        ownerApp: OWNER_APP,
        projectSchema: SQLITE_PROJECT,
        dialect: "sqlite",
        nameFallback: migration.name,
      }).envelope,
  );
}

/** The `plan` half: read-only reconciliation, which is what the CLI's live plan runs. */
async function sqlitePlan(
  driver: DriverConfig,
  migrations: readonly NamedMigration[],
  registry: Record<string, string>,
) {
  return await statusEnvelopes({
    ownerApp: OWNER_APP,
    projectSchema: SQLITE_PROJECT,
    driver,
    registry,
    policy: [noInjectPolicy(SQLITE_PROJECT)],
    envelopes: sqliteEnvelopes(migrations),
    readOnly: true,
  });
}

/** The `apply` half: the base migration, then the migration under test with the base
 *  as a non-empty prior so both verbs see the identical authored set. */
async function sqliteApply(
  driver: DriverConfig,
  migration: NamedMigration,
  registry: Record<string, string>,
) {
  const base = baseMigration();
  await apply({
    migration: base,
    ownerApp: OWNER_APP,
    projectSchema: SQLITE_PROJECT,
    driver,
    registry,
    policy: [noInjectPolicy(SQLITE_PROJECT)],
    approved: true,
    nameFallback: base.name,
  });
  return await apply({
    migration,
    priorMigrations: [base],
    priorNameFallbacks: [base.name],
    ownerApp: OWNER_APP,
    projectSchema: SQLITE_PROJECT,
    driver,
    registry,
    policy: [noInjectPolicy(SQLITE_PROJECT)],
    approved: true,
    nameFallback: migration.name,
  });
}

/** The declared type the live SQLite catalog holds for a column. `pragma_table_info`
 *  rows arrive null-prototyped, so the field is copied out rather than compared as a
 *  whole row. */
function sqliteColumnType(appPath: string, column: string): string | undefined {
  const db = new DatabaseSync(appPath);
  try {
    const row = db
      .prepare(`SELECT name, type FROM pragma_table_info('${TABLE}')`)
      .all()
      .find((candidate) => (candidate as { name: string }).name === column);
    return row === undefined ? undefined : (row as { type: string }).type;
  } finally {
    db.close();
  }
}

/** The versions the SQLite journal reports. Absent journal file means nothing ran. */
function sqliteCompletedVersions(journalPath: string): string[] {
  const db = new DatabaseSync(journalPath);
  try {
    const named = db
      .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'")
      .all();
    if (named.length === 0) return [];
    return db
      .prepare("SELECT version FROM schema_migrations ORDER BY version")
      .all()
      .map((row) => (row as { version: string }).version);
  } finally {
    db.close();
  }
}

/**
 * A fresh SQLite app database with `notes` seeded OUT OF BAND at the given column
 * type, so the table is in the catalog and NOT in the migration history.
 *
 * The declaration under test is `t.string({ length: 64 })`, which SQLite renders as
 * `TEXT`; a seed of `INTEGER` is the divergent shape.
 */
function withSeededSqlite(
  bodySqlType: string,
  body: (driver: DriverConfig) => Promise<void>,
): Promise<void> {
  const directory = mkdtempSync(join(tmpdir(), "zm-guard-fold-sqlite-"));
  const driver = sqliteDriverAt(directory);
  const db = new DatabaseSync(driver.kind === "sqlite" ? driver.appPath : "");
  db.exec(`CREATE TABLE "${TABLE}" ("id" INTEGER PRIMARY KEY NOT NULL, "body" ${bodySqlType});`);
  db.close();
  return body(driver).finally(() => {
    rmSync(directory, { recursive: true, force: true });
  });
}

/** `notes(id, body TEXT)` with the guard, matching the out-of-band seed exactly. */
function sqliteNotes(name: string, guarded: boolean): NamedMigration {
  return createNotes(name, { guarded, body: () => t.string({ length: 64 }) });
}

test("SQLite: plan and apply agree that a guarded createTable over a matching live table proceeds", async () => {
  await withSeededSqlite("TEXT", async (driver) => {
    const guarded = sqliteNotes("guard_fold_sqlite_adopt", true);
    const registry = { [BASE_TABLE]: OWNER_APP, [TABLE]: OWNER_APP };

    // Plan runs FIRST, against a database with no journal at all, which is the state
    // a user plans from. Read-only reconciliation must not bootstrap one.
    const planned = await sqlitePlan(driver, [baseMigration(), guarded], registry);
    assert.equal(
      planned.pending.length,
      2,
      "the projection let the guarded op through: both migrations are planned as pending",
    );

    await sqliteApply(driver, guarded, registry);

    assert.equal(
      sqliteColumnType(driver.kind === "sqlite" ? driver.appPath : "", "body"),
      "TEXT",
      "the live table is untouched: the guard proved equality and the CREATE was skipped",
    );
    assert.equal(
      sqliteCompletedVersions(driver.kind === "sqlite" ? driver.journalPath : "").length,
      2,
      "apply reached the same verdict as plan and journaled both migrations",
    );
  });
});

test("SQLite: plan and apply agree in refusing a guarded createTable over a divergent live table", async () => {
  await withSeededSqlite("INTEGER", async (driver) => {
    const guarded = sqliteNotes("guard_fold_sqlite_drift", true);
    const registry = { [BASE_TABLE]: OWNER_APP, [TABLE]: OWNER_APP };

    // Requiring the divergence in BOTH messages is the point. "already exists" would
    // also be a refusal, but it is the guard-blind one: it fires on name presence and
    // would let a wrong-shaped table be journaled completed.
    await assert.rejects(
      sqlitePlan(driver, [baseMigration(), guarded], registry),
      /existence-guard drift[\s\S]*notes\.body/,
      "the plan path must refuse a divergent live shape and say what diverged",
    );
    await assert.rejects(
      sqliteApply(driver, guarded, registry),
      /existence-guard drift[\s\S]*notes\.body/,
      "the apply path must refuse the same set the same way",
    );

    assert.equal(
      sqliteColumnType(driver.kind === "sqlite" ? driver.appPath : "", "body"),
      "INTEGER",
      "the refusal changed nothing: the live column keeps its own type",
    );
    const completed = sqliteCompletedVersions(
      driver.kind === "sqlite" ? driver.journalPath : "",
    );
    assert.equal(
      completed.length,
      1,
      `only the base migration is journaled; the refused one left no completed row (got ${JSON.stringify(completed)})`,
    );
  });
});

test("SQLite control: an UNGUARDED createTable over an existing live table is still refused by plan", async () => {
  await withSeededSqlite("TEXT", async (driver) => {
    // The ONLY difference from the adopting arm is `ifNotExists`. Without this arm,
    // the adopting arm alone cannot distinguish "the projection honours the guard"
    // from "the projection stopped detecting duplicates on SQLite".
    const bare = sqliteNotes("guard_fold_sqlite_bare", false);
    await assert.rejects(
      sqlitePlan(driver, [baseMigration(), bare], {
        [BASE_TABLE]: OWNER_APP,
        [TABLE]: OWNER_APP,
      }),
      /failed to project pending schema[\s\S]*already exists/,
      "an unguarded duplicate create must still be refused by the projection",
    );
  });
});
