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
// Does NOT cover MySQL (`mysql_canonical_type` folds `varchar(N)` to `text`, so the
// MySQL guard never compared the length in the first place).
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
const OWNER_APP = "app_guard_varchar_adopt";
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
