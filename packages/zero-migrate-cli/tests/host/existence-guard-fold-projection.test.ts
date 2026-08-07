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
// Five arms, because "the projection honours the guard" and "the projection stopped
// detecting duplicates" look identical from one:
//   - adopt: live table matches the declaration EXACTLY and the incoming registry
//     names this app as the owner -- MUST apply cleanly;
//   - divergent: the same guarded op over a live table whose column width differs --
//     MUST still refuse, and the refusal must name the DIVERGENCE, not the name
//     collision, because a projection that skipped on the fold's error kind would
//     skip on NAME presence alone and journal a completed row over a wrong shape;
//   - unowned: the same guarded op with the table ABSENT from the incoming ownership
//     registry -- MUST refuse. Dropping the op would also silently hand this app
//     ownership of a table another app created, then journal it completed, after
//     which nothing re-examines it;
//   - unguarded control: the same createTable WITHOUT `ifNotExists` -- MUST still
//     refuse, which isolates the guard as what changed rather than duplicate
//     detection in general;
//   - MySQL: the guarded op MUST refuse exactly as it does today. MySQL evaluates
//     no existence probe at apply time, so letting a SatisfiedNoop through the
//     projection would send bare DDL to a backend that cannot check anything.
//
// Every arm drives the REAL path: authored through the public `zero-migrate` API,
// lowered by the native addon, applied through `zero-migrate-cli`'s `apply()` over the
// real `pg`/`mysql2` driver seams against a live database, with `priorMigrations`
// non-empty so the folding path is the one under test, and with the pre-existing
// table created OUT OF BAND. Seeding it through the engine would put it in the
// migration HISTORY as well as the catalog and would measure a different question.
//
// Does NOT cover SQLite (the Node host DriverConfig has no SQLite seam), does NOT
// cover guarded ops other than `createTable`, and does NOT cover a partially matching
// table whose secondary index is still absent.

import assert from "node:assert/strict";
import { test } from "node:test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { apply, type DriverConfig, type MigrationModule } from "zero-migrate-cli";
import { table, t, type ColumnDef } from "zero-migrate";
import { noInjectPolicy } from "./policy.js";

const HERE = dirname(fileURLToPath(import.meta.url));

if (!process.env.ZERO_MIGRATE_ADDON_PATH) {
  const { platform, arch } = process;
  const abi = platform === "linux" ? "-gnu" : "";
  process.env.ZERO_MIGRATE_ADDON_PATH = join(
    HERE,
    `../../../../crates/zero-migrate-node/zero-migrate-node.${platform}-${arch}${abi}.node`,
  );
}

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;
const OWNER_APP = "app_guard_fold_projection";
const BASE_TABLE = "guard_fold_base";
const TABLE = "notes";

type NamedMigration = MigrationModule & { readonly name: string };

function authoredMigration(name: string, up: () => void): NamedMigration {
  return { name, default: { up } } as NamedMigration;
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

test("PostgreSQL: a guarded createTable is refused when the incoming registry does not own the table", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; guarded projection ownership e2e skipped");
    return;
  }
  await withAppliedBaseAndSeededTable(
    "guardfold_unowned_pg",
    "varchar(255)",
    async (client, schema, driver, meta) => {
      // Identical to the adopting arm except that `notes` is absent from the incoming
      // registry, i.e. some OTHER app created it. Skipping the op would drop it from
      // the pending set AND claim ownership unconditionally, then journal completed --
      // a loud recoverable refusal turned into a silent permanent ownership transfer.
      const guarded = createNotes("guard_fold_unowned", { guarded: true, body: () => t.string() });
      await assert.rejects(
        applyAfterBase(guarded, schema, driver, { [BASE_TABLE]: OWNER_APP }),
        /ownership/,
        "a guarded create over a table this project does not own must refuse, not adopt",
      );

      const completed = await pgCompletedVersions(client, meta);
      assert.equal(
        completed.length,
        1,
        `only the base migration is journaled; the unowned adoption left no completed row (got ${JSON.stringify(completed)})`,
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
