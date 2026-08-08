// Existence-guard index lookup vs. the dialect's index-name SCOPE, end to end.
//
// The guard probe resolves a `createIndex ifNotExists` by index NAME. An index name
// is schema-wide on PostgreSQL and SQLite but PER TABLE on MySQL, so a same-name
// index on a DIFFERENT table means different things per dialect. Treating it as
// "the declared index already exists" skips the CREATE and journals the migration
// complete while the declared index was never created.
//
// Every arm drives the REAL path: authored through the public `zero-migrate` API,
// lowered by the native addon, applied over the real pg/mysql2 driver seam, then
// read back from the live catalog (`pg_index` / `information_schema.STATISTICS`)
// rather than from an engine return value.
//
// Does NOT cover SQLite here. That is a placement choice, not a seam that is
// missing: the Node host `DriverConfig` DOES carry
// `{ kind: "sqlite"; appPath; journalPath }`
// (`packages/zero-migrate-cli/src/index.ts:73`), `apply()` routes it to
// `applyIrSqlite`, and `existence-guard-fold-projection.test.ts` in this directory
// drives live SQLite arms through exactly that. The SQLite arm for THIS question is
// caught in `crates/zero-migrate/tests/existence_guard_sqlite.rs`
// (`create_index_ifnotexists_name_owned_by_another_table_fails_closed`), against a
// real SQLite catalog in process.
//
// Does NOT cover expression or partial indexes, which the probe already fails closed
// on (`existence_probe.rs` `FailDrift` naming `expression`).

import assert from "node:assert/strict";
import { test } from "node:test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { apply, type DriverConfig, type MigrationModule } from "zero-migrate-cli";
import { table, t } from "zero-migrate";
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
const OWNER_APP = "app_guard_index_scope";
const SHARED_INDEX = "idx_shared";
const FREE_INDEX = "idx_free";
const TABLE_A = "guard_idx_a";
const TABLE_B = "guard_idx_b";

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

function ownership(...tables: string[]): Record<string, string> {
  return Object.fromEntries(tables.map((name) => [name, OWNER_APP]));
}

/** `guard_idx_a` owns `idx_shared`; `guard_idx_b` carries the same column and no
 *  index of its own. The indexed column is `int` so MySQL needs no key prefix. */
function baseMigration(): NamedMigration {
  return authoredMigration("guard_index_scope_base", () => {
    table(TABLE_A).create({
      columns: { id: t.int().notNull(), bucket: t.int() },
      primaryKey: ["id"],
      indexes: [{ name: SHARED_INDEX, on: ["bucket"] }],
    });
    table(TABLE_B).create({
      columns: { id: t.int().notNull(), bucket: t.int() },
      primaryKey: ["id"],
    });
  });
}

/** A guarded create on `guard_idx_b` under a name of the caller's choosing. With
 *  `SHARED_INDEX` the name is already owned by `guard_idx_a`; with `FREE_INDEX` it
 *  is free: the only variable between the negative and its control. */
function guardedCreateOnB(migrationName: string, indexName: string): NamedMigration {
  return authoredMigration(migrationName, () => {
    table(TABLE_B).index(indexName).add({ on: ["bucket"], ifNotExists: true });
  });
}

async function applyInitial(
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
    appliedBy: "guard-index-scope-e2e",
    nameFallback: migration.name,
  });
}

async function applyAfter(
  prior: NamedMigration,
  migration: NamedMigration,
  projectSchema: string,
  driver: DriverConfig,
) {
  return apply({
    migration,
    priorMigrations: [prior],
    priorNameFallbacks: [prior.name],
    ownerApp: OWNER_APP,
    projectSchema,
    driver,
    registry: ownership(TABLE_A, TABLE_B),
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "guard-index-scope-e2e",
    nameFallback: migration.name,
  });
}

/** `(index, table)` pairs the live PostgreSQL catalog holds for one schema. */
async function pgIndexOwners(
  client: import("pg").Client,
  schema: string,
): Promise<Array<{ index: string; table: string }>> {
  const result = await client.query(
    `SELECT i.relname AS index, tbl.relname AS table
       FROM pg_index x
       JOIN pg_class i ON i.oid = x.indexrelid
       JOIN pg_class tbl ON tbl.oid = x.indrelid
       JOIN pg_namespace n ON n.oid = i.relnamespace
      WHERE n.nspname = $1
      ORDER BY 1`,
    [schema],
  );
  return result.rows as Array<{ index: string; table: string }>;
}

/** The index names the live MySQL catalog reports for one table. */
async function mysqlIndexNames(
  admin: import("mysql2/promise").Connection,
  database: string,
  tableName: string,
): Promise<string[]> {
  const [rows] = await admin.query(
    `SELECT DISTINCT INDEX_NAME AS name
       FROM information_schema.STATISTICS
      WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
      ORDER BY INDEX_NAME`,
    [database, tableName],
  );
  return (rows as Array<{ name: string }>).map((row) => row.name);
}

async function withPgSchema(
  prefix: string,
  body: (client: import("pg").Client, schema: string) => Promise<void>,
): Promise<void> {
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueNamespace(prefix);
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await body(client, schema);
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

test("PostgreSQL: a guarded createIndex is refused, not skipped, when another table owns the name", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL index-name-scope e2e skipped");
    return;
  }
  await withPgSchema("guardidx_taken_pg", async (client, schema) => {
    const base = baseMigration();
    const driver = { kind: "postgres" as const, url: PG_URL };
    await applyInitial(base, schema, driver);
    assert.deepEqual(
      (await pgIndexOwners(client, schema)).find((row) => row.index === SHARED_INDEX),
      { index: SHARED_INDEX, table: TABLE_A },
      `${TABLE_A} owns ${SHARED_INDEX} before the guarded create`,
    );

    const collide = guardedCreateOnB("guard_index_scope_collide", SHARED_INDEX);
    await assert.rejects(
      applyAfter(base, collide, schema, driver),
      /existence-guard drift[\s\S]*index idx_shared[\s\S]*guard_idx_a/,
      "the guarded create must fail closed on a name another table owns",
    );

    const owners = await pgIndexOwners(client, schema);
    assert.deepEqual(
      owners.filter((row) => row.index === SHARED_INDEX),
      [{ index: SHARED_INDEX, table: TABLE_A }],
      `${SHARED_INDEX} still belongs to ${TABLE_A} only; the refusal changed nothing`,
    );
    assert.ok(
      !owners.some((row) => row.table === TABLE_B && row.index === SHARED_INDEX),
      `${TABLE_B} carries no ${SHARED_INDEX}: a silent no-op would have left this ` +
        "same catalog state behind a SUCCESSFUL apply, which is the defect",
    );
  });
});

test("PostgreSQL control: the same guarded createIndex still runs when the name is free", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL index-name-scope control skipped");
    return;
  }
  await withPgSchema("guardidx_free_pg", async (client, schema) => {
    const base = baseMigration();
    const driver = { kind: "postgres" as const, url: PG_URL };
    await applyInitial(base, schema, driver);

    // The ONLY difference from the arm above is the index name. If the fix had
    // broken the guarded create path wholesale rather than the cross-table lookup,
    // this control would go red with it.
    const free = guardedCreateOnB("guard_index_scope_free", FREE_INDEX);
    await applyAfter(base, free, schema, driver);

    assert.deepEqual(
      (await pgIndexOwners(client, schema)).find((row) => row.index === FREE_INDEX),
      { index: FREE_INDEX, table: TABLE_B },
      `${TABLE_B} carries ${FREE_INDEX}: an unclaimed name still reaches the CREATE`,
    );
  });
});

test("MySQL: a guarded createIndex lands on its own table under a name another table also uses", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL index-name-scope e2e skipped");
    return;
  }
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({
    uri: MYSQL_URL,
    multipleStatements: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  });
  const database = uniqueNamespace("guardidx_my");
  const meta = `${database}_migrations`;
  const base = baseMigration();
  const collide = guardedCreateOnB("guard_index_scope_collide", SHARED_INDEX);
  const driver = { kind: "mysql" as const, url: MYSQL_URL };

  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await applyInitial(base, database, driver);
    assert.ok(
      (await mysqlIndexNames(admin, database, TABLE_A)).includes(SHARED_INDEX),
      `${TABLE_A} carries ${SHARED_INDEX} before the guarded create`,
    );
    assert.ok(
      !(await mysqlIndexNames(admin, database, TABLE_B)).includes(SHARED_INDEX),
      `${TABLE_B} does not carry ${SHARED_INDEX} before the guarded create`,
    );

    await applyAfter(base, collide, database, driver);

    // MySQL scopes index names per table, so both tables legitimately carry one.
    assert.ok(
      (await mysqlIndexNames(admin, database, TABLE_B)).includes(SHARED_INDEX),
      `${TABLE_B} carries its own ${SHARED_INDEX}: a same-name index on ` +
        `${TABLE_A} is an unrelated object on MySQL and must not satisfy the guard`,
    );
    assert.ok(
      (await mysqlIndexNames(admin, database, TABLE_A)).includes(SHARED_INDEX),
      `${TABLE_A} keeps its own ${SHARED_INDEX}`,
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
