// Adopting an EXISTING PostgreSQL table whose column is a length-qualified
// `varchar(N)`, through a guarded `createTable ifNotExists`.
//
// The guard proves shape equality by comparing the DECLARED column `data_type`
// against the LIVE one. The declared side spells a `t.string({ length })` column
// `character varying(N)`; the live side is read from `information_schema`, whose
// `data_type` is the bare base name `character varying` with the length carried
// separately in `character_maximum_length`. When the snapshot drops that length the
// two spellings can never match, so every length-qualified varchar column fails
// closed on adoption -- and `t.string()` DEFAULTS to `length: 255`, so that is the
// default string type.
//
// Three arms, because "adoption is broken" and "length-qualified adoption is broken"
// look identical from one:
//   - varchar: live `varchar(255)` adopted by a declared `t.string()` -- MUST be clean;
//   - text control: live `text` adopted by a declared `t.text()` -- differs ONLY in the
//     column type, and already passes, so it isolates the length qualifier as the cause;
//   - divergent: live `varchar(255)` declared `t.string({ length: 100 })` -- MUST still
//     fail closed. Without it a "fix" that stops comparing the field at all would pass
//     the first two arms.
//
// Every arm drives the REAL path: authored through the public `zero-migrate` API,
// lowered by the native addon, applied through `zero-migrate-cli`'s `apply()` over the
// real `pg` driver seam against a live database, with the pre-state created OUT OF BAND
// so the adoption is genuine rather than a re-run of the engine's own DDL.
//
// MySQL is covered at the bottom, and the reason it needed covering is that this
// comment used to say the wrong thing. It read: "does NOT cover MySQL
// (`mysql_canonical_type` folds `varchar(N)` to `text`, so the MySQL guard never
// compared the length in the first place)" - which describes a guard that would
// ADOPT a divergent column silently, the worst outcome available, and would have
// sent the next reader hunting a bug that does not exist.
//
// Measured, it is the opposite. MySQL refuses the guarded adoption outright, with
// `<unknown: MySQL column-type equality ...>` on the live side, and it refuses
// EVEN WHEN THE DECLARED TYPE MATCHES. That is what `support-matrix.md` footnote 1
// means by "any decision requiring column-type equality is refused until
// modifier-preserving equality is implemented": presence-only guards work on MySQL
// (an `ifExists` drop does), but a `createTable ifNotExists` over an existing table
// always needs column-type equality, so guarded ADOPTION is unavailable there.
//
// Does NOT cover SQLite, and the reason is the same class as MySQL's, not a missing
// seam: the SQLite leg of `existence_probe::decide` canonicalises BOTH sides through
// `schema::query::sqlite_canonical_type`, whose fallback arm maps every unrecognised
// spelling -- `character varying` and `character varying(255)` alike -- to the `text`
// affinity, so no length ever reaches the compare and the defect this file pins
// cannot arise there. NOTHING STRUCTURAL BLOCKS A SQLITE ARM: the Node host
// `DriverConfig` DOES carry `{ kind: "sqlite"; appPath; journalPath }`
// (`packages/zero-migrate-cli/src/index.ts:73`), `apply()` routes it to
// `applyIrSqlite`, and `existence-guard-fold-projection.test.ts` in this same
// directory drives live SQLite arms through it.
//
// Does NOT cover `character(N)` (already recovered by the snapshot, by the
// `character`-only arm this one widens).
//
// Does NOT cover the stand-alone `addColumn ifNotExists` probe shape: no arm here or
// elsewhere drives one over a length-qualified column. The recomposition it would
// depend on lives in the SHARED snapshot builder (`crates/zero-migrate/src/apply/
// drift.rs`) that every probe shape reads, so the fix is not table-shaped -- but the
// addColumn shape itself is unpinned end to end, which is a hole, not a handoff.

import assert from "node:assert/strict";
import { test } from "node:test";

import { apply, type DriverConfig, type MigrationModule } from "zero-migrate-cli";
import { table, t, type ColumnDef } from "zero-migrate";
import { noInjectPolicy } from "./policy.js";


// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const OWNER_APP = "app_guard_varchar_adopt";
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

/** The guarded adoption: `createTable ifNotExists` declaring `notes(id, body)` with
 *  `body` typed by the caller. The declared shape is the ONLY variable between the
 *  arms; the live table is identical in every other respect. */
function guardedCreate(name: string, body: () => ColumnDef): NamedMigration {
  return authoredMigration(name, () => {
    table(TABLE).create({
      columns: { id: t.int().notNull(), body: body() },
      primaryKey: ["id"],
      ifNotExists: true,
    });
  });
}

async function applyGuarded(
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
    appliedBy: "guard-varchar-adoption-e2e",
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
  return result.rows[0] as { data_type: string; character_maximum_length: number | null } | undefined;
}

/** Create the schema, seed `notes` OUT OF BAND with `body` of the given SQL type, and
 *  run the body against it. The out-of-band CREATE is what makes this an ADOPTION: the
 *  table was not written by this engine, so nothing but the catalog proves its shape. */
async function withSeededTable(
  prefix: string,
  bodySqlType: string,
  body: (client: import("pg").Client, schema: string, driver: DriverConfig) => Promise<void>,
): Promise<void> {
  const pg = (await import("pg")).default;
  const client = new pg.Client({ connectionString: PG_URL });
  await client.connect();
  const schema = uniqueNamespace(prefix);
  const meta = `${schema}_migrations`;
  try {
    await client.query(`CREATE SCHEMA ${pgIdent(schema)}`);
    await client.query(
      `CREATE TABLE ${pgIdent(schema)}.${pgIdent(TABLE)} (
         id integer PRIMARY KEY NOT NULL,
         body ${bodySqlType}
       )`,
    );
    await body(client, schema, { kind: "postgres", url: PG_URL! });
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

test("PostgreSQL: a guarded createTable adopts an existing length-qualified varchar column", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; varchar adoption e2e skipped");
    return;
  }
  await withSeededTable("guardvarchar_adopt_pg", "varchar(255)", async (client, schema, driver) => {
    assert.deepEqual(
      await pgColumnType(client, schema, "body"),
      { data_type: "character varying", character_maximum_length: 255 },
      "the live catalog reports the bare base name plus the length in a SEPARATE column: " +
        "the length is present in the catalog, so a snapshot that omits it discarded it",
    );

    // `t.string()` defaults to `length: 255`, so this declares exactly the live shape.
    await applyGuarded(
      guardedCreate("guard_varchar_adopt", () => t.string()),
      schema,
      driver,
    );

    assert.deepEqual(
      await pgColumnType(client, schema, "body"),
      { data_type: "character varying", character_maximum_length: 255 },
      "the adopted column is untouched: the guard proved equality and skipped the CREATE",
    );
  });
});

test("PostgreSQL control: the same guarded createTable adopts an existing text column", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; text adoption control skipped");
    return;
  }
  await withSeededTable("guardvarchar_text_pg", "text", async (client, schema, driver) => {
    // The ONLY difference from the arm above is the column type -- an unqualified one.
    // This arm passing while that one fails is what proves the LENGTH QUALIFIER is the
    // cause rather than adoption in general; it also guards against a fix that breaks
    // adoption wholesale.
    await applyGuarded(
      guardedCreate("guard_text_adopt", () => t.text()),
      schema,
      driver,
    );

    assert.deepEqual(
      await pgColumnType(client, schema, "body"),
      { data_type: "text", character_maximum_length: null },
      "the adopted text column is untouched",
    );
  });
});

test("PostgreSQL: a guarded createTable still fails closed on a genuinely divergent varchar length", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; varchar divergence e2e skipped");
    return;
  }
  await withSeededTable("guardvarchar_drift_pg", "varchar(255)", async (client, schema, driver) => {
    // Live varchar(255), declared varchar(100). Recovering the length is only a fix if
    // a DIFFERENT length is still a refusal: a "fix" that stopped comparing `data_type`,
    // or that dropped the length from BOTH sides, would silently adopt a column that
    // truncates at a different width. That is the failure this arm exists to catch.
    await assert.rejects(
      applyGuarded(
        guardedCreate("guard_varchar_drift", () => t.string({ length: 100 })),
        schema,
        driver,
      ),
      // Naming BOTH widths is the point: before the fix this arm also refused, but
      // because the live side reported a bare `character varying`. Requiring the
      // message to carry `(100)` against `(255)` proves the refusal is now about the
      // WIDTHS DIFFERING rather than about the length having gone missing.
      /existence-guard drift[\s\S]*column notes\.body[\s\S]*data_type[\s\S]*character varying\(100\)[\s\S]*character varying\(255\)/,
      "a declared width that differs from the live width must be refused fail-closed",
    );

    assert.deepEqual(
      await pgColumnType(client, schema, "body"),
      { data_type: "character varying", character_maximum_length: 255 },
      "the refusal changed nothing: the live column keeps its own width",
    );
  });
});

// ── MySQL: guarded adoption is refused, matching type or not ─────────────────
//
// The three arms above are PostgreSQL's, where the guard compares declared against
// live and the interesting question is whether the width survives the snapshot. On
// MySQL there is no comparison to get right: the live side canonicalises to
// `<unknown: MySQL column-type equality ...>`, so every column-type decision is
// refused before it can be wrong.
//
// Both arms matter, and the MATCHING one matters more. "A divergent type is
// refused" is what you would expect of a working comparison, so on its own it
// cannot tell a fail-closed engine from a comparing one. The matching arm is what
// shows there is no comparison at all - a declared type IDENTICAL to the live one
// is refused just the same - which is the operational fact an operator needs:
// guarded adoption of an existing table does not work on MySQL, and no choice of
// declared type makes it work.
//
// GATE: `ZERO_MIGRATE_MYSQL_URL`.

const MYSQL_URL = process.env.ZERO_MIGRATE_MYSQL_URL;

function mysqlIdent(value: string): string {
  return `\`${value.replaceAll("`", "``")}\``;
}

function mysqlCharter(database: string): string {
  const scope = `{ include = [${JSON.stringify(database)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}
`;
}

/** Seed `notes` OUT OF BAND with `body` of the given MySQL type, then run `body`. */
async function withSeededMysqlTable(
  prefix: string,
  bodySqlType: string,
  body: (
    admin: Awaited<ReturnType<typeof import("mysql2/promise").createConnection>>,
    database: string,
    driver: DriverConfig,
  ) => Promise<void>,
): Promise<void> {
  const mysql = (await import("mysql2/promise")).default;
  const admin = await mysql.createConnection({ uri: MYSQL_URL, multipleStatements: true });
  const database = uniqueNamespace(prefix);
  try {
    await admin.query(`CREATE DATABASE ${mysqlIdent(database)}`);
    await admin.query(
      `CREATE TABLE ${mysqlIdent(database)}.${mysqlIdent(TABLE)} (
         id int PRIMARY KEY NOT NULL,
         body ${bodySqlType}
       ) ENGINE=InnoDB`,
    );
    await body(admin, database, { kind: "mysql", url: MYSQL_URL! });
  } finally {
    await admin
      .query(
        `DROP DATABASE IF EXISTS ${mysqlIdent(database)};
         DROP DATABASE IF EXISTS ${mysqlIdent(`${database}_migrations`)}`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
}

/** The `(DATA_TYPE, CHARACTER_MAXIMUM_LENGTH)` the live MySQL catalog holds. */
async function mysqlColumnType(
  admin: Awaited<ReturnType<typeof import("mysql2/promise").createConnection>>,
  database: string,
): Promise<{ DATA_TYPE: string; CHARACTER_MAXIMUM_LENGTH: number | null } | undefined> {
  const [rows] = await admin.query(
    `SELECT DATA_TYPE, CHARACTER_MAXIMUM_LENGTH FROM information_schema.COLUMNS
      WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = 'body'`,
    [database, TABLE],
  );
  return (rows as Array<{ DATA_TYPE: string; CHARACTER_MAXIMUM_LENGTH: number | null }>)[0];
}

test("MySQL: a guarded createTable refuses a divergent varchar width fail-closed", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL guarded adoption skipped");
    return;
  }
  await withSeededMysqlTable("guard_my_drift", "varchar(255)", async (admin, database, driver) => {
    await assert.rejects(
      applyGuarded(
        guardedCreate("guard_my_drift", () => t.string({ length: 100 })),
        database,
        driver,
      ),
      /existence-guard drift[\s\S]*unknown: MySQL column-type equality/,
      "MySQL must refuse rather than compare, and say so",
    );
    assert.deepEqual(
      await mysqlColumnType(admin, database),
      { DATA_TYPE: "varchar", CHARACTER_MAXIMUM_LENGTH: 255 },
      "the refusal changed nothing about the live column",
    );
  });
});

test("MySQL: the SAME guard refuses even when the declared type matches exactly", async (ctx) => {
  if (!MYSQL_URL) {
    ctx.skip("ZERO_MIGRATE_MYSQL_URL unset; MySQL guarded adoption skipped");
    return;
  }
  // The arm that carries the finding. A refusal here cannot be a comparison
  // deciding the types differ, because they do not - it is the absence of any
  // comparison, failing closed. Nothing else in this suite says that.
  await withSeededMysqlTable("guard_my_match", "varchar(100)", async (admin, database, driver) => {
    await assert.rejects(
      applyGuarded(
        guardedCreate("guard_my_match", () => t.string({ length: 100 })),
        database,
        driver,
      ),
      /existence-guard drift[\s\S]*unknown: MySQL column-type equality/,
      "an exactly-matching declared type must be refused too: MySQL never compares",
    );
    assert.deepEqual(
      await mysqlColumnType(admin, database),
      { DATA_TYPE: "varchar", CHARACTER_MAXIMUM_LENGTH: 100 },
      "the refusal changed nothing about the live column",
    );
  });
});
