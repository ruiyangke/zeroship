// Dropping a masked column removes its `<col>_masked` sibling too, end to end.
//
// A masked column is TWO physical columns: the declared one and a `<col>_masked`
// sibling the engine injects, carrying a `zero-migrate:mask` sentinel COMMENT. One
// authored `create` makes both - `lower_create_table` reconciles siblings through
// `ensure_create_table_masked_siblings`, and a masked `addColumn` lowers the sibling
// as a second unit.
//
// The mirror op did not. `Op::DropColumn` in crates/zero-migrate/src/render/lower.rs
// emitted a single unit for the named column and nothing for the sibling, so
// dropping a masked column left an orphan behind: a column with a mask sentinel on
// it, belonging to a field that no longer exists.
//
// Nothing in this repository collects that orphan. The declarative differ would
// propose dropping it (`schema/diff.rs` classifies a live-but-undeclared column as a
// destructive DropColumn), but that differ does not run here: its live-schema reader
// `read_live_schema` sits behind `#[cfg(feature = "introspect")]`, a feature
// deliberately never declared (`crates/zero-migrate/build.rs:16-24` calls those
// helpers "permanently-off dead code"), and `compute_diff` has no caller outside its
// own in-module tests. So the orphan is permanent.
//
// Both halves are asserted, because either alone is the wrong story: the catalog
// (the sibling is gone) AND the journal (the migration recorded applied, so this is
// not a refusal masquerading as a fix).
//
// PostgreSQL only. The sibling mechanism is dialect-neutral, but this asserts against
// `information_schema.columns`; the SQLite shape is NOT covered here and NOT covered
// elsewhere - a gap, not a handoff.

import assert from "node:assert/strict";
import { test } from "node:test";

import { apply, type DriverConfig, type MigrationModule } from "zero-migrate-cli";
import { table, t } from "zero-migrate";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const OWNER_APP = "app_masked_drop";
const TABLE = "masked_drop_people";
const MASKED_COLUMN = "ssn";
const SIBLING = `${MASKED_COLUMN}_masked`;

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

/** One table whose `ssn` column carries a standalone mask, so the engine injects the
 *  `ssn_masked` sibling beside it. */
function createMasked(): NamedMigration {
  return authoredMigration("masked_drop_base", () => {
    table(TABLE).create({
      columns: {
        id: t.int().notNull(),
        [MASKED_COLUMN]: t.string().mask({ kind: "last4" }),
      },
      primaryKey: ["id"],
    });
  });
}

/** The mirror of the op that created the pair. */
function dropMasked(): NamedMigration {
  return authoredMigration("masked_drop_remove", () => {
    table(TABLE).column(MASKED_COLUMN).drop({});
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
    appliedBy: "masked-drop-e2e",
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
    registry: { [TABLE]: OWNER_APP },
    policy: [noInjectPolicy(projectSchema)],
    approved: true,
    appliedBy: "masked-drop-e2e",
    nameFallback: migration.name,
  });
}

/** The column names the live catalog holds for the table. */
async function pgColumns(
  client: import("pg").Client,
  schema: string,
): Promise<string[]> {
  const result = await client.query(
    `SELECT column_name FROM information_schema.columns
      WHERE table_schema = $1 AND table_name = $2
      ORDER BY column_name`,
    [schema, TABLE],
  );
  return (result.rows as Array<{ column_name: string }>).map((row) => row.column_name);
}

/** What the journal itself records, read from the server. */
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

test("PostgreSQL: dropping a masked column takes its _masked sibling with it", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset; PostgreSQL masked-drop e2e skipped");
    return;
  }
  await withPgSchema("maskeddrop_pg", async (client, schema) => {
    const base = createMasked();
    const driver = { kind: "postgres" as const, url: PG_URL };
    await applyInitial(base, schema, driver);

    // Proves the fixture actually produced a masked column. Without this the drop
    // assertion below would pass on a table that never had a sibling to orphan.
    const created = await pgColumns(client, schema);
    assert.ok(
      created.includes(MASKED_COLUMN) && created.includes(SIBLING),
      `the masked create makes BOTH columns; got ${JSON.stringify(created)}`,
    );

    const before = await pgJournalPhases(client, schema);
    await applyAfter(base, dropMasked(), schema, driver);

    const after = await pgColumns(client, schema);
    const journal = await pgJournalPhases(client, schema);
    const landed = journal
      .slice(before.length)
      .map((row) => `${row.name}:${row.event_kind}/${row.phase}`)
      .join(", ");

    assert.ok(
      !after.includes(MASKED_COLUMN),
      `the declared column is gone; got ${JSON.stringify(after)}`,
    );
    assert.ok(
      journal.length > before.length,
      `the drop journaled something (new rows: ${landed || "<none>"}), so this is an ` +
        "applied migration rather than a refusal",
    );
    assert.ok(
      !after.includes(SIBLING),
      `the ${SIBLING} sibling went with its parent, but the catalog still holds ` +
        `${JSON.stringify(after)} and the journal recorded [${landed}]. One authored op ` +
        "created the pair, so one authored op removes it; an orphan carrying a " +
        "zero-migrate:mask sentinel belongs to no declared field and nothing in this " +
        "repository collects it.",
    );
  });
});
