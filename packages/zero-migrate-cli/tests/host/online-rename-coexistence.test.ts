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

function authored(name: string, up: () => void): NamedMigration {
  return { name, default: { up } } as NamedMigration;
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

  const created = authored("create_users", () => {
    table("users").create({
      columns: { id: t.int().notNull(), display_name: t.string({ length: 255 }) },
      primaryKey: ["id"],
    });
    table("users").insert({ rows: { id: 1, display_name: "original" } });
  });
  const renamed = authored("rename_display_name", () => {
    table("users").column("display_name").rename({
      to: "full_name",
      type: t.string({ length: 255 }),
    });
  });
  const later = authored("add_extra", () => {
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
