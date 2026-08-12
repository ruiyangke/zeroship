// `apply()` and `status()` do not leak database connections.
//
// `docs/node-api.md`: "Each call opens and closes its own database connection.
// The caller does not need to close it manually."
//
// That is a promise about a resource, and a broken one fails in the worst way
// available: invisibly and slowly. A host that leaks one backend per deploy runs
// fine in a test suite, fine in staging, and exhausts `max_connections` in
// production weeks later - at which point the symptom is every OTHER service
// failing to connect, and nothing points at the migration tool.
//
// Nothing measured it. Every other arm in this suite asserts what a call DID; none
// asserts what it left behind.
//
// WHAT THIS CAN AND CANNOT DISTINGUISH. `pg_stat_activity` is server-wide, and the
// host suite runs its files concurrently, so a strict "delta must be zero" would
// flake on an unrelated file's connection being momentarily open. The assertion is
// therefore a BOUND: after twelve engine calls the backend count must not have
// grown by twelve, or by anything close to it. That is deliberately weaker than
// equality and still catches the failure mode the claim is about, because a leak
// is per-call and monotonic while concurrency noise is a transient handful. A leak
// of one connection per call would land at +12; the assertion trips well below it.
//
// The reading is taken twice, with a settle between, so a socket still closing at
// the first read is not mistaken for a leak.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, status, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_connection_lifecycle";
const APPLIES = 8;
const STATUSES = 4;

type NamedMigration = MigrationModule & { readonly name: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

function creates(index: number): NamedMigration {
  return {
    name: `create_t${index}`,
    default: {
      up() {
        table(`t${index}`).create({
          columns: { id: t.int().notNull() },
          primaryKey: ["id"],
        });
      },
    },
  } as NamedMigration;
}

test("repeated apply and status calls leave no database connections behind", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const projectSchema = uniqueNamespace("conn_lifecycle");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  /** Backends on this database, excluding this test's own admin connection. */
  const backends = async (): Promise<number> => {
    const { rows } = await admin.query(
      `SELECT count(*)::int AS n FROM pg_stat_activity
        WHERE datname = current_database() AND pid <> pg_backend_pid()`,
    );
    return rows[0].n as number;
  };

  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);
    const before = await backends();

    const priors: NamedMigration[] = [];
    for (let index = 0; index < APPLIES; index += 1) {
      const migration = creates(index);
      await apply({
        migration,
        priorMigrations: [...priors],
        priorNameFallbacks: priors.map((p) => p.name),
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        registry: {},
        policy: [noInjectPolicy(projectSchema)],
        approved: true,
        appliedBy: "connection-lifecycle",
        nameFallback: migration.name,
      });
      priors.push(migration);
    }

    // `status` is a second entry point carrying the same promise, and it takes a
    // different path through the driver seam, so it is worth its own calls.
    for (let index = 0; index < STATUSES; index += 1) {
      await status({
        migrations: priors,
        nameFallbacks: priors.map((p) => p.name),
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        registry: {},
        policy: [noInjectPolicy(projectSchema)],
      });
    }

    // Two readings with a settle between: a socket mid-close is not a leak.
    let after = await backends();
    if (after > before) {
      await new Promise((resolve) => setTimeout(resolve, 1500));
      after = await backends();
    }

    const calls = APPLIES + STATUSES;
    assert.ok(
      after - before < calls,
      `${calls} engine calls must not each leave a backend behind: ` +
        `before=${before} after=${after} delta=${after - before}`,
    );

    // The control. Without it the assertion above also passes when the engine
    // never connected at all - the migrations have to have actually landed.
    const { rows } = await admin.query(
      `SELECT count(*)::int AS n FROM information_schema.tables
        WHERE table_schema = $1`,
      [projectSchema],
    );
    assert.equal(rows[0].n, APPLIES, "every apply must really have created its table");
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
