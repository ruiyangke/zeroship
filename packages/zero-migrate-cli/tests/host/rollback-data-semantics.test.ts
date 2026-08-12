// What a rollback does to DATA, against live PostgreSQL.
//
// `rollback-live.test.ts` covers the schema side - the object goes away, the journal
// records it, a second unwind is a no-op, a targetless or unapproved one is refused.
// None of it asks what happens to the ROWS, and that is the question an operator
// planning a rollback actually has.
//
// It went undocumented long enough that `docs/operations.md` denied the verb existed
// at all. When that was corrected I nearly replaced one guess with another - that
// unwinding a `dropColumn` hands back an empty column, the way a naive inverse
// would. Measuring it says otherwise, and the engine is safer than the guess:
//
//   1. unwinding an ADDITIVE migration removes what it added and leaves surviving
//      rows untouched;
//   2. unwinding a DESTRUCTIVE one is REFUSED as irreversible by default, rather
//      than re-creating an empty object that reads like a restore;
//   3. `force` + `backupAcknowledged` SKIPS the irreversible migration and reports
//      it in `skippedIrreversible` - it does not fabricate a reverse.
//
// Arm 3 is the one worth having. "Force" is the flag an operator reaches for under
// pressure, and the difference between "skips it" and "reverses it approximately"
// is the difference between a schema that still has the drop and one that quietly
// pretends the data came back.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, rollback, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_rollback_data";

type NamedMigration = MigrationModule & { readonly name: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

function authored(name: string, up: () => void): NamedMigration {
  return { name, default: { up } } as NamedMigration;
}

/** Seeds a table carrying one row, so every arm has data to lose. */
function seedWithARow(): NamedMigration {
  return authored("seed", () => {
    table("acct").create({
      columns: { id: t.int().notNull(), secret: t.string({ length: 20 }) },
      primaryKey: ["id"],
    });
    table("acct").insert({ rows: [{ id: 1, secret: "hunter2" }] });
  });
}

test("a rollback removes what a migration added and keeps the rows it did not touch", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const projectSchema = uniqueNamespace("rbdata_add");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };
  const seed = seedWithARow();
  const added = authored("add_extra", () => {
    table("acct").column("extra").add({ type: t.string({ length: 10 }) });
  });

  const columns = async (): Promise<string[]> =>
    (
      await admin.query(
        `SELECT column_name FROM information_schema.columns
          WHERE table_schema = $1 AND table_name = 'acct' ORDER BY ordinal_position`,
        [projectSchema],
      )
    ).rows.map((r) => r.column_name as string);
  const rows = async (): Promise<unknown[]> =>
    (await admin.query(`SELECT id, secret FROM ${pgIdent(projectSchema)}.acct ORDER BY id`)).rows;

  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);
    const base = {
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      policy: [noInjectPolicy(projectSchema)],
      approved: true,
      appliedBy: "rollback-data-semantics",
    };
    await apply({ ...base, migration: seed, priorMigrations: [], priorNameFallbacks: [], registry: {}, nameFallback: seed.name });
    await apply({
      ...base,
      migration: added,
      priorMigrations: [seed],
      priorNameFallbacks: [seed.name],
      registry: { acct: OWNER_APP },
      nameFallback: added.name,
    });
    assert.deepEqual(await columns(), ["id", "secret", "extra"], "the add landed");

    const outcome = await rollback({
      migrations: [seed, added],
      nameFallbacks: [seed.name, added.name],
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: { acct: OWNER_APP },
      policy: [noInjectPolicy(projectSchema)],
      target: { kind: "steps", steps: 1 },
      approved: true,
      backupAcknowledged: true,
      appliedBy: "rollback-data-semantics",
    });

    assert.equal(outcome.skippedIrreversible.length, 0, "an additive migration is reversible");
    assert.deepEqual(await columns(), ["id", "secret"], "the added column is gone");
    assert.deepEqual(
      await rows(),
      [{ id: 1, secret: "hunter2" }],
      "the row the migration never touched survives the unwind",
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

test("a rollback refuses a dropped column rather than handing back an empty one, and force skips instead of faking", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const projectSchema = uniqueNamespace("rbdata_drop");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };
  const seed = seedWithARow();
  const dropped = authored("drop_secret", () => {
    table("acct").column("secret").drop();
  });

  const columns = async (): Promise<string[]> =>
    (
      await admin.query(
        `SELECT column_name FROM information_schema.columns
          WHERE table_schema = $1 AND table_name = 'acct' ORDER BY ordinal_position`,
        [projectSchema],
      )
    ).rows.map((r) => r.column_name as string);

  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);
    const base = {
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      policy: [noInjectPolicy(projectSchema)],
      approved: true,
      appliedBy: "rollback-data-semantics",
    };
    await apply({ ...base, migration: seed, priorMigrations: [], priorNameFallbacks: [], registry: {}, nameFallback: seed.name });
    await apply({
      ...base,
      migration: dropped,
      priorMigrations: [seed],
      priorNameFallbacks: [seed.name],
      registry: { acct: OWNER_APP },
      nameFallback: dropped.name,
    });
    assert.deepEqual(await columns(), ["id"], "the drop landed");

    const unwind = (force: boolean) =>
      rollback({
        migrations: [seed, dropped],
        nameFallbacks: [seed.name, dropped.name],
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        registry: { acct: OWNER_APP },
        policy: [noInjectPolicy(projectSchema)],
        target: { kind: "steps", steps: 1 },
        approved: true,
        force,
        backupAcknowledged: true,
        appliedBy: "rollback-data-semantics",
      });

    // Default: refused, and the message names the migration and recommends the
    // forward fix rather than leaving the operator to guess.
    await assert.rejects(
      unwind(false),
      /is irreversible \(down: None\); rollback refuses by default/,
      "a dropped column has no reverse and the engine says so",
    );
    assert.deepEqual(await columns(), ["id"], "the refused unwind changed nothing");

    // Forced: skipped, not fabricated. The dropped column stays dropped.
    const forced = await unwind(true);
    assert.equal(forced.rolledBack.length, 0, "nothing was unwound");
    assert.equal(
      forced.skippedIrreversible.length,
      1,
      `the irreversible migration is reported, got ${JSON.stringify(forced)}`,
    );
    assert.deepEqual(
      await columns(),
      ["id"],
      "force does NOT re-create the dropped column as an empty one",
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
