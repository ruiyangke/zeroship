// An UNGUARDED `createTable` whose INLINE index names one another table already
// owns, end to end.
//
// `unguarded-index-name-collision.test.ts` covers the same collision reached through
// `Op::CreateIndex`, which lowering stamps with an ownership-only probe when the
// dialect scopes index names schema-wide. A `createTable` carrying `indexes: [...]`
// does not go through that op. Its inline indexes are emitted inside
// `lower_create_table` (`crates/zero-migrate/src/render/declarative.rs`), whose
// per-index probe is stamped only `if let Some(dir) = guard` - so an UNGUARDED
// create attaches no probe at all, while the emitter still writes
// `CREATE INDEX IF NOT EXISTS`. The name is skipped by the server and the migration
// journals complete.
//
// Every arm drives the REAL path: authored through the public `zero-migrate` API,
// lowered by the native addon, applied over the real pg driver seam, then read back
// from `pg_index` and from the journal table rather than from an engine return
// value.
//
// PostgreSQL only, deliberately. The exposure needs an index name scoped wider than
// its table, which is what `Capability::SchemaWideIndexNames` names; MySQL scopes
// index names per table, writes no `IF NOT EXISTS`, and evaluates no probe, so there
// is nothing here to cover. SQLite shares the schema-wide scoping and is covered for
// the `createIndex` shape in `crates/zero-migrate/tests/existence_guard_sqlite.rs`;
// the createTable shape on SQLite is NOT covered by this file and NOT covered there
// either - a gap, not a handoff.
//
// Does NOT cover a collision the same migration UNIT creates before the statement
// runs, and nothing else does: the probe reads one catalog snapshot per unit, and
// the fold's duplicate-index check keys on the target table's own index list, so it
// never asks which OTHER table owns a name. That widening was rejected on purpose
// (review-log F48).

import assert from "node:assert/strict";
import { test } from "node:test";

import { apply, type DriverConfig, type MigrationModule } from "zero-migrate-cli";
import { table, t } from "zero-migrate";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const OWNER_APP = "app_inline_index";
const SHARED_INDEX = "idx_inline_shared";
const FREE_INDEX = "idx_inline_free";
const TABLE_A = "inline_idx_a";
const TABLE_C = "inline_idx_c";

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

function ownership(...tables: string[]): Record<string, string> {
  return Object.fromEntries(tables.map((name) => [name, OWNER_APP]));
}

/** `inline_idx_a` owns `idx_inline_shared`. Nothing else exists yet. */
function baseMigration(): NamedMigration {
  return authoredMigration("inline_index_base", () => {
    table(TABLE_A).create({
      columns: { id: t.int().notNull(), bucket: t.int() },
      primaryKey: ["id"],
      indexes: [{ name: SHARED_INDEX, on: ["bucket"] }],
    });
  });
}

/** An UNGUARDED `createTable` carrying an INLINE index: no `ifNotExists` anywhere.
 *  With `SHARED_INDEX` the name is already owned by `inline_idx_a`; with
 *  `FREE_INDEX` it is free. The index name is the only variable between the
 *  negative arm and its control. */
function createTableWithInlineIndex(
  migrationName: string,
  indexName: string,
): NamedMigration {
  return authoredMigration(migrationName, () => {
    table(TABLE_C).create({
      columns: { id: t.int().notNull(), bucket: t.int() },
      primaryKey: ["id"],
      indexes: [{ name: indexName, on: ["bucket"] }],
    });
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
    appliedBy: "inline-index-e2e",
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
    registry: ownership(TABLE_A, TABLE_C),
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "inline-index-e2e",
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

/** What the journal records, read from the server: the other half of the defect
 *  pair. A silently skipped inline create leaves a `completed` `applied` row. */
async function pgJournalPhases(
  client: import("pg").Client,
  schema: string,
): Promise<Array<{ name: string; event_kind: string; phase: string | null }>> {
  const result = await client.query(
    `SELECT name, event_kind, phase
       FROM ${pgIdent(`${schema}_migrations`)}.schema_migrations
      ORDER BY event_seq`,
  );
  return result.rows as Array<{ name: string; event_kind: string; phase: string | null }>;
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

test("PostgreSQL: an unguarded createTable is refused when its inline index names another table's", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL inline-index e2e skipped");
    return;
  }
  await withPgSchema("inlineidx_taken_pg", async (client, schema) => {
    const base = baseMigration();
    const driver = { kind: "postgres" as const, url: PG_URL };
    await applyInitial(base, schema, driver);
    assert.deepEqual(
      (await pgIndexOwners(client, schema)).find((row) => row.index === SHARED_INDEX),
      { index: SHARED_INDEX, table: TABLE_A },
      `${TABLE_A} owns ${SHARED_INDEX} before the colliding createTable`,
    );

    const before = await pgJournalPhases(client, schema);
    const collide = createTableWithInlineIndex("inline_index_collide", SHARED_INDEX);
    const settled = await applyAfter(base, collide, schema, driver).then(
      () => null,
      (error: unknown) => error,
    );

    const owners = await pgIndexOwners(client, schema);
    const onC = owners.some((row) => row.table === TABLE_C && row.index === SHARED_INDEX);
    const after = await pgJournalPhases(client, schema);
    const landed = after
      .slice(before.length)
      .map((row) => `${row.name}:${row.event_kind}/${row.phase}`)
      .join(", ");

    if (settled === null) {
      // The defect, stated as the pair the server reports: the journal grew a
      // completed row, and the index that row claims to have created sits on the
      // wrong table or nowhere at all.
      assert.fail(
        `apply RESOLVED and the journal grew [${landed || "<no new row>"}] while ` +
          `${TABLE_C} carries ${SHARED_INDEX}: ${onC}. A green journal over an inline index ` +
          `that was never created is the defect: PostgreSQL scopes ${SHARED_INDEX} ` +
          `schema-wide, ${TABLE_A} owns it, and the emitted IF NOT EXISTS made the inline ` +
          `create a no-op.`,
      );
    }

    assert.match(
      String((settled as Error).message),
      /existence-guard drift[\s\S]*idx_inline_shared[\s\S]*inline_idx_a/,
      "the refusal must name the index and the table that owns it",
    );
    assert.deepEqual(
      owners.filter((row) => row.index === SHARED_INDEX),
      [{ index: SHARED_INDEX, table: TABLE_A }],
      `${SHARED_INDEX} still belongs to ${TABLE_A} only; the refusal changed nothing`,
    );
  });
});

test("PostgreSQL control: the same createTable still runs when its inline index name is free", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL inline-index control skipped");
    return;
  }
  await withPgSchema("inlineidx_free_pg", async (client, schema) => {
    const base = baseMigration();
    const driver = { kind: "postgres" as const, url: PG_URL };
    await applyInitial(base, schema, driver);

    // The ONLY difference from the arm above is the index name. Without this the
    // negative arm would pass just as well against a createTable path broken
    // outright.
    const free = createTableWithInlineIndex("inline_index_free", FREE_INDEX);
    await applyAfter(base, free, schema, driver);

    const owners = await pgIndexOwners(client, schema);
    assert.deepEqual(
      owners.find((row) => row.index === FREE_INDEX),
      { index: FREE_INDEX, table: TABLE_C },
      `${TABLE_C} carries ${FREE_INDEX}: an unclaimed inline name still reaches the CREATE`,
    );
    assert.ok(
      owners.some((row) => row.table === TABLE_C && row.index !== FREE_INDEX),
      `${TABLE_C} also carries its primary-key index, so the table itself was created ` +
        "rather than the arm passing on an empty catalog",
    );
  });
});
