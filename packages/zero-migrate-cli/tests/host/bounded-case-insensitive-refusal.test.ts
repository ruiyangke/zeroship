// The bounded-plus-case-insensitive string, asked at the front door.
//
// `t.string({ length, caseSensitive: false })` is expressible: `StringOptions` takes
// both facets in one bag and the recorder emits both. The engine refuses it -
// `caseSensitive: false` is valid only on a text column - and this asks `apply`
// rather than any renderer, because the only question worth pinning is what happens
// to a user who writes it.
//
// This exists because I once concluded the opposite. Rendering that shape directly
// through `build_resolved_table_snapshot` produces `varchar(32)` on PostgreSQL and
// `TEXT ... utf8mb4_0900_ai_ci` on MySQL, and those really do behave differently on
// live servers: PostgreSQL accepted a case-variant pair into a UNIQUE column, MySQL
// stored 40 characters in a column authored 32. Every one of those numbers was real,
// and none of them was evidence about an authored migration, because that renderer
// sits BENEATH the gate and no authored migration reaches it.
//
// Both arms use the REAL server. An unreachable DSN would look like it worked - the
// bounded arm would still reject, on `ECONNREFUSED` - and the control would still
// pass while proving nothing, because validation is not reached before the driver
// connects. That was the first draft of this file, and it passed.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_bounded_ci";

type NamedMigration = MigrationModule & { readonly name: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

function migrationDeclaring(name: string, column: () => unknown): NamedMigration {
  return {
    name,
    default: {
      up() {
        table("contacts").create({
          columns: { email: column() as never },
        });
      },
    },
  } as NamedMigration;
}

test("a bounded string cannot be case-insensitive, and the refusal names the facet", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const projectSchema = uniqueNamespace("boundedci_pg");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const applyOne = (migration: NamedMigration) =>
    apply({
      migration,
      priorMigrations: [],
      priorNameFallbacks: [],
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: {},
      policy: [noInjectPolicy(projectSchema)],
      approved: true,
      appliedBy: "bounded-ci-refusal",
      nameFallback: migration.name,
    });

  const tableExists = async (): Promise<boolean> => {
    const { rows } = await admin.query(
      `SELECT 1 FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = $1 AND c.relname = 'contacts'`,
      [projectSchema],
    );
    return rows.length === 1;
  };

  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);

    await assert.rejects(
      applyOne(
        migrationDeclaring("bounded_case_insensitive", () =>
          t.string({ length: 32, caseSensitive: false }),
        ),
      ),
      /caseSensitive:false is only valid on a text column/,
      "a bounded case-insensitive string must be refused with the facet named",
    );

    // Refused BEFORE anything was created, not half-applied and rolled back into a
    // shape that happens to look clean.
    assert.equal(await tableExists(), false, "the refused migration created nothing");

    // The control, against the SAME live server: an unbounded text column applies,
    // so the refusal above is specific to the length bound rather than to anything
    // about this migration, this policy, or this schema.
    //
    // The control deliberately does NOT carry `caseSensitive: false`, even though
    // that is the spelling the refusal recommends. On PostgreSQL that renders
    // `public.citext`, which is a contrib extension a stock server does not install,
    // so the recommended spelling fails at apply with `type "public.citext" does not
    // exist` on any database that has not created it first. That is its own defect
    // and it is pinned in `citext-prerequisite.test.ts`; borrowing it as the control
    // here would couple this refusal to an unrelated failure.
    await applyOne(migrationDeclaring("unbounded_text", () => t.text()));
    assert.equal(await tableExists(), true, "an unbounded text column applies");
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
