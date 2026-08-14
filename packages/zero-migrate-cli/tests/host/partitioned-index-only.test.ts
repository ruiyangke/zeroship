// `ON ONLY` on a partitioned parent index, authored in TypeScript and measured
// against live PostgreSQL.
//
// F412 - `IndexSnapshot.only` was compared by both the drift attribute pass and
// index equality, while introspection reports a constant `false`. A `createIndex`
// that authored `only: true` therefore reported structural drift from the moment it
// applied and never stopped. `only` is now emission-only, alongside `opclass` and
// `nulls_not_distinct`.
//
// The fix rests on one measured fact about the server, and that fact is what this
// file pins: `pg_get_indexdef` renders `ON ONLY` for EVERY index whose table is
// partitioned, whether or not `ONLY` was written. If that stopped being true -
// if the catalog ever started distinguishing the two - then `only` would become
// recoverable and excluding it from the comparison would be the wrong call. No
// offline test can notice that change; this one fails.
//
// The arm deliberately authors BOTH spellings against the same server in the same
// run. The authored-plain index is the control: without it, `ON ONLY` appearing on
// the authored-only index would read as the catalog faithfully recording intent,
// which is precisely the reading the measurement refutes.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";
import { partitionPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_partition_index_only";
const ASKED_ONLY = "only_asked_parent";
const ASKED_PLAIN = "only_plain_parent";

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

/** Two partitioned parents that differ ONLY in whether their index asked for
 *  `only`. Everything else about them is identical on purpose. */
function twoParentsOneAskingForOnly(): NamedMigration {
  return authoredMigration("partition_index_only", () => {
    for (const parent of [ASKED_ONLY, ASKED_PLAIN]) {
      table(parent).create({
        columns: { bucket: t.int().notNull(), payload: t.string().notNull() },
        partitionBy: { range: ["bucket"] },
      });
    }
    table(ASKED_ONLY)
      .index(`${ASKED_ONLY}_payload_idx`)
      .add({ on: [{ column: "payload" }], only: true });
    table(ASKED_PLAIN)
      .index(`${ASKED_PLAIN}_payload_idx`)
      .add({ on: [{ column: "payload" }] });
  });
}

test("PostgreSQL renders ON ONLY for every partitioned-parent index, so the flag is not recoverable", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const projectSchema = uniqueNamespace("partonly_pg");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const indexDef = async (index: string): Promise<string> => {
    const { rows } = await admin.query(
      `SELECT pg_get_indexdef(i.indexrelid) AS def
         FROM pg_catalog.pg_index i
         JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = $2`,
      [projectSchema, index],
    );
    assert.equal(rows.length, 1, `the index ${index} exists in ${projectSchema}`);
    return rows[0].def as string;
  };

  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);

    await apply({
      migration: twoParentsOneAskingForOnly(),
      priorMigrations: [],
      priorNameFallbacks: [],
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: {},
      policy: [partitionPolicy(projectSchema)],
      approved: true,
      appliedBy: "partitioned-index-only-e2e",
      nameFallback: "partition_index_only",
    });

    const asked = await indexDef(`${ASKED_ONLY}_payload_idx`);
    const plain = await indexDef(`${ASKED_PLAIN}_payload_idx`);

    assert.ok(
      asked.includes(" ON ONLY "),
      `an index authored only:true renders ON ONLY, got ${JSON.stringify(asked)}`,
    );

    // The control, and the whole point of the file. The plain index never asked for
    // ONLY and the catalog spells it identically, so the definition carries no
    // evidence of what was authored.
    assert.ok(
      plain.includes(" ON ONLY "),
      `an index authored WITHOUT only also renders ON ONLY, got ${JSON.stringify(plain)}`,
    );
  } finally {
    await admin
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(projectSchema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(meta)} CASCADE`,
      )
      .catch(() => {});
    await admin.end().catch(() => {});
  }
});
