// A rollback's teardown and its journal event commit together, or neither does.
//
// `postgres/session.rs` states it as a guarantee: "Atomic: the `down` + its
// `rolled_back` journal append commit together". Nothing measured it, and the
// engine's fault-injection seam has no rollback point, so a crash mid-rollback
// had no coverage on any path.
//
// It is the guarantee that keeps `status` honest across a failed rollback. Break
// it and the two halves come apart in whichever order the code happens to run:
// teardown committed, journal not, so the schema is gone while history still says
// `applied` - and the next deploy plans against a table that no longer exists,
// with nothing anywhere reporting drift.
//
// THE DATABASE IS THE FAULT INJECTOR. No engine seam is needed: a CHECK
// constraint on the journal's own `event_kind` makes the `rolled_back` append -
// and only that append - fail, inside the transaction the teardown already ran
// in. If the two are atomic, the teardown disappears with it. That is a sharper
// probe than a crash would be, because it fails exactly one statement rather than
// the whole process, so a passing result cannot be explained by "nothing had run
// yet".
//
// The second test covers the neighbouring trap found while building the first: an
// authored `down()` is REFUSED rather than ignored. Rollback runs the engine's
// synthesised inverse, so an author who writes a `down()` body and is not told
// would watch a teardown they did not write run against their schema.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, rollback, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_rollback_atomicity";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function charter(schema: string): string {
  const scope = `{ include = [${JSON.stringify(schema)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

/** No `down()`: the engine synthesises the inverse from the recorded ops. */
const migration = {
  name: "create_widgets",
  default: {
    up() {
      table("widgets").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    },
  },
} as MigrationModule;

test("a failed journal append takes the teardown down with it", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const projectSchema = uniqueNamespace("rb_atomic");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };
  const policy = [charter(projectSchema)];

  const widgetsExists = async (): Promise<boolean> => {
    const { rows } = await client.query(
      `SELECT EXISTS (
         SELECT 1 FROM information_schema.tables
          WHERE table_schema = $1 AND table_name = 'widgets') AS present`,
      [projectSchema],
    );
    return rows[0].present as boolean;
  };

  const journalKinds = async (): Promise<string[]> => {
    const { rows } = await client.query(
      `SELECT event_kind FROM "${meta}".schema_migrations ORDER BY event_kind`,
    );
    return rows.map((row) => row.event_kind as string);
  };

  const unwind = () =>
    rollback({
      migrations: [migration],
      nameFallbacks: [migration.name!],
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: {},
      policy,
      target: { kind: "steps", steps: 1 },
      approved: true,
      backupAcknowledged: true,
      appliedBy: "rollback-atomicity",
    });

  try {
    await client.query(`CREATE SCHEMA "${projectSchema}"`);
    await apply({
      migration,
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: {},
      policy,
      approved: true,
      appliedBy: "rollback-atomicity",
      nameFallback: migration.name!,
    });
    assert.equal(await widgetsExists(), true, "the migration must have created its table");

    // Fail the journal's `rolled_back` append, and nothing else.
    await client.query(
      `ALTER TABLE "${meta}".schema_migrations
         ADD CONSTRAINT no_rb CHECK (event_kind <> 'rolled_back')`,
    );

    await assert.rejects(
      unwind(),
      /violates check constraint "no_rb"/,
      "the rollback must fail when its journal append cannot commit",
    );

    // The assertion this file exists for. The teardown ran BEFORE the append in
    // the same transaction, so if the two were not atomic the table would be
    // gone by now while the journal still said `applied`.
    assert.equal(
      await widgetsExists(),
      true,
      "the teardown must have rolled back with the failed journal append",
    );
    assert.deepEqual(
      await journalKinds(),
      ["applied"],
      "no partial journal state may survive the failed rollback",
    );

    // THE CONTROL. Remove the induced fault and the same call must succeed -
    // otherwise the arm above is equally consistent with a rollback that simply
    // never works, and the atomicity claim would be untested either way.
    await client.query(`ALTER TABLE "${meta}".schema_migrations DROP CONSTRAINT no_rb`);
    await unwind();
    assert.equal(await widgetsExists(), false, "a clean rollback must remove the table");
    assert.deepEqual(
      await journalKinds(),
      ["applied", "rolled_back"],
      "and must append exactly one rolled_back event",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${projectSchema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
  }
});

test("an authored down() is refused, not silently ignored", async () => {
  // Rollback runs the engine's SYNTHESISED inverse. An author who writes a
  // `down()` body and is not told would watch a teardown they did not write run
  // against their schema - so the refusal, and the fact that it names the reason,
  // is the safety property.
  //
  // `apply()` is async, so this is a REJECTED PROMISE rather than a synchronous
  // throw - `assert.throws` passes vacuously against it, which is how the first
  // draft of this arm failed.
  await assert.rejects(
    () =>
      apply({
        migration: {
          name: "with_down",
          default: {
            up() {
              table("widgets").create({
                columns: { id: t.int().notNull() },
                primaryKey: ["id"],
              });
            },
            down() {
              table("widgets").drop();
            },
          },
        } as MigrationModule,
        ownerApp: OWNER_APP,
        projectSchema: "public",
        driver: { kind: "postgres", url: pgUrl() },
        registry: {},
        policy: [charter("public")],
        approved: true,
        appliedBy: "rollback-atomicity",
        nameFallback: "with_down",
      }),
    /down\(\) function, which the recorder does not capture/,
    "an authored down() must be refused with the reason, before any connection is opened",
  );
});
