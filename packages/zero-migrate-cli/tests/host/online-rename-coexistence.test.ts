// The security model's coexistence guarantees for a PostgreSQL online rename,
// measured against a live server.
//
// `docs/security-model.md` makes four precise promises about the window between
// starting a rename and resolving it. They are the promises an application relies on
// while it moves from the old column name to the new one, and getting any of them
// wrong diverges data silently rather than failing:
//
//   1. the destination starts out carrying the source's values;
//   2. a write through the SOURCE name keeps both aligned;
//   3. a write through the DESTINATION name keeps both aligned;
//   4. "if both receive different values in one statement, the destination wins".
//
// Plus the guarantee that protects the whole window: "Other migration changes to the
// table are blocked until an approved apply or abort resolution succeeds. This
// prevents a later schema change from racing an unresolved application transition."
//
// None of this was covered. `pg_scenarios.rs` exercises backfills and the rename
// machinery, but nothing anywhere wrote through BOTH names in one statement - the
// case where the promise is a tie-break rather than a copy, and the only one where a
// wrong answer picks the wrong data.
//
// These assert through the DATABASE, not through the engine's reply: the guarantee is
// about what a plain SQL write from application code sees, and application code does
// not go through the engine.
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

const OWNER_APP = "app_rename_coexist";

type NamedMigration = MigrationModule & { readonly name: string };

function authoredMixedMigration(name: string, up: () => void): NamedMigration {
  return { name, default: { up } } as NamedMigration;
}

function authoredSchemaMigration(name: string, schema: () => void): NamedMigration {
  return { name, default: { schema } } as NamedMigration;
}

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

test("during an online rename both names stay aligned, and the destination wins a tie", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const projectSchema = uniqueNamespace("coexist");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const created = authoredMixedMigration("create_users", () => {
    table("users").create({
      columns: { id: t.int().notNull(), display_name: t.string({ length: 255 }) },
      primaryKey: ["id"],
    });
    table("users").insert({ rows: { id: 1, display_name: "original" } });
  });
  const renamed = authoredSchemaMigration("rename_display_name", () => {
    table("users").column("display_name").rename({
      to: "full_name",
      type: t.string({ length: 255 }),
    });
  });
  const later = authoredSchemaMigration("add_extra", () => {
    table("users").column("extra").add({ type: t.string({ length: 8 }) });
  });

  const applyOne = (migration: NamedMigration, priors: NamedMigration[], registry = {}) =>
    apply({
      migration,
      priorMigrations: priors,
      priorNameFallbacks: priors.map((p) => p.name),
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry,
      policy: [noInjectPolicy(projectSchema)],
      approved: true,
      appliedBy: "online-rename-coexistence",
      nameFallback: migration.name,
    });

  /** Both column values as the DATABASE sees them, which is what an application sees. */
  const bothNames = async (): Promise<{ source: string | null; destination: string | null }> => {
    const { rows } = await admin.query(
      `SELECT display_name, full_name FROM ${pgIdent(projectSchema)}.users WHERE id = 1`,
    );
    assert.equal(rows.length, 1, "the seeded row is present");
    return { source: rows[0].display_name, destination: rows[0].full_name };
  };

  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);
    await applyOne(created, []);
    await applyOne(renamed, [created], { users: OWNER_APP });

    // 1. The destination starts out carrying the source's values.
    assert.deepEqual(
      await bothNames(),
      { source: "original", destination: "original" },
      "the rename's backfill must copy the source into the destination",
    );

    // 2. A write through the SOURCE name reaches the destination.
    await admin.query(
      `UPDATE ${pgIdent(projectSchema)}.users SET display_name = 'via_source' WHERE id = 1`,
    );
    assert.deepEqual(
      await bothNames(),
      { source: "via_source", destination: "via_source" },
      "a write through the source name must keep both aligned",
    );

    // 3. A write through the DESTINATION name reaches the source.
    await admin.query(
      `UPDATE ${pgIdent(projectSchema)}.users SET full_name = 'via_destination' WHERE id = 1`,
    );
    assert.deepEqual(
      await bothNames(),
      { source: "via_destination", destination: "via_destination" },
      "a write through the destination name must keep both aligned",
    );

    // 4. The tie-break, and the assertion nothing else in the repo makes. Both names
    //    are assigned DIFFERENT values in ONE statement; the destination wins, and
    //    the source is dragged to the destination's value rather than keeping its own.
    await admin.query(
      `UPDATE ${pgIdent(projectSchema)}.users
          SET display_name = 'source_value', full_name = 'destination_value'
        WHERE id = 1`,
    );
    assert.deepEqual(
      await bothNames(),
      { source: "destination_value", destination: "destination_value" },
      "when one statement assigns both names, the DESTINATION value must win on both",
    );

    // 5. The guarantee that protects the window: no other change to this table lands
    //    until the rename resolves, so a later schema change cannot race the
    //    application's cutover.
    await assert.rejects(
      applyOne(later, [created, renamed], { users: OWNER_APP }),
      /is not fully applied \(state: partial\)/,
      "a later migration touching the renamed table must be blocked until resolution",
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

// What the destination column does NOT inherit, and what still binds anyway.
//
// `docs/node-api.md`: "The rename does not transfer `NOT NULL`, defaults, unique
// or primary-key rules, indexes, comments, or dependent objects."
//
// Measured, that is exactly right - the destination comes out nullable, with no
// default, no unique constraint, no index and no comment. But an operator reading
// only that sentence draws the wrong conclusion, because it describes the COLUMN
// and not the WRITES. The dual-write trigger copies every write to the source, so
// the source's constraints still bind writes made through the DESTINATION name:
//
//   UPDATE users SET full_name = NULL
//     -> null value in column "display_name" ... violates not-null constraint
//   INSERT ... (full_name) VALUES ('original')
//     -> duplicate key value violates unique constraint "users_display_name_key"
//
// So an application that has fully cut over to the new name still cannot write
// values the OLD column would reject, and the error it gets NAMES A COLUMN ITS
// CODE NO LONGER MENTIONS. That is the surprising half, and it is the half worth
// pinning: "the destination is nullable" and "you may now write NULL" look like
// the same statement and are not.
//
// This is safe behaviour, not a defect - constraints staying enforced across the
// window is what keeps the source usable for rollback. The fixture exists so the
// asymmetry is measured rather than inferred.

test("during coexistence the source's constraints still bind writes made through the destination", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;

  const projectSchema = uniqueNamespace("coexist_constraints");
  const meta = `${projectSchema}_migrations`;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const created = authoredMixedMigration("create_users", () => {
    table("users").create({
      columns: {
        id: t.int().notNull(),
        display_name: t.string({ length: 255 }).notNull().default("anon"),
      },
      primaryKey: ["id"],
      uniques: [{ name: "users_display_name_key", columns: ["display_name"] }],
    });
    table("users").insert({ rows: { id: 1, display_name: "original" } });
  });
  const renamed = authoredSchemaMigration("rename_display_name", () => {
    table("users").column("display_name").rename({
      to: "full_name",
      type: t.string({ length: 255 }),
    });
  });

  const applyOne = (migration: NamedMigration, priors: NamedMigration[], registry = {}) =>
    apply({
      migration,
      priorMigrations: priors,
      priorNameFallbacks: priors.map((p) => p.name),
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry,
      policy: [noInjectPolicy(projectSchema)],
      approved: true,
      appliedBy: "online-rename-coexistence",
      nameFallback: migration.name,
    });

  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);
    await applyOne(created, []);
    await applyOne(renamed, [created], { users: OWNER_APP });

    // 1. The documented half: the destination inherits none of it.
    const { rows } = await admin.query(
      `SELECT column_name, is_nullable, column_default
         FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'users'
          AND column_name IN ('display_name', 'full_name')
        ORDER BY column_name`,
      [projectSchema],
    );
    assert.deepEqual(
      rows.map((row) => [row.column_name, row.is_nullable, row.column_default === null]),
      [
        ["display_name", "NO", false],
        ["full_name", "YES", true],
      ],
      "the destination must be nullable and default-less while the source keeps both",
    );

    // 2. The half the sentence does not say. Both writes go through the NEW name
    //    and both are rejected by a constraint on the OLD one.
    await assert.rejects(
      admin.query(`UPDATE ${pgIdent(projectSchema)}.users SET full_name = NULL WHERE id = 1`),
      /null value in column "display_name".*violates not-null constraint/s,
      "a NULL written through the destination must still hit the source's NOT NULL",
    );
    await assert.rejects(
      admin.query(
        `INSERT INTO ${pgIdent(projectSchema)}.users (id, full_name) VALUES (2, 'original')`,
      ),
      /violates unique constraint "users_display_name_key"/,
      "a duplicate written through the destination must still hit the source's UNIQUE",
    );

    // 3. The control. Both refusals above must be the SOURCE's constraints biting,
    //    not the destination being unwritable: a legal write still succeeds.
    await admin.query(
      `UPDATE ${pgIdent(projectSchema)}.users SET full_name = 'renamed' WHERE id = 1`,
    );
    const after = await admin.query(
      `SELECT display_name, full_name FROM ${pgIdent(projectSchema)}.users WHERE id = 1`,
    );
    assert.deepEqual(
      [after.rows[0].display_name, after.rows[0].full_name],
      ["renamed", "renamed"],
      "a legal write through the destination still lands on both columns",
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
