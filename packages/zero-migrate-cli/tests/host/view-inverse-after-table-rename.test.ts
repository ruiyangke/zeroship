// The CARRIER question: does a real deploy's rollback actually render a dropped
// view's inverse from a folded history, and does a table rename reach that body?
//
// The Rust side proves the property (`crates/zero-migrate/tests/rollback/
// drop_view_rollback_pg.rs::a_table_rename_reaches_the_body_a_dropped_view_is_restored_from`),
// but it builds its own `LiveSchema` from `fold_ops`. That is the right SHAPE and it
// is still a claim about what the host does. This file removes the claim: it goes
// through `rollback()` from `zero-migrate-cli`, which is the napi `rollback` export
// (`crates/zero-migrate-node/src/bridge.rs:809`) -> `rollback_with_locked_backend`
// -> `lower_ordered_envelopes_to_plans_for_rollback`
// (`crates/zero-migrate-node/src/lower.rs:368`). That function replays the executed
// history from an EMPTY snapshot and merges the recovered object definitions into a
// catalog-sourced live schema (`merge_recovered_definitions`), because the catalog
// cannot show a view that has already been dropped. `ViewSnapshot::authored_query`
// arrives there from the fold and nowhere else - a PostgreSQL catalog read leaves it
// `None`, which is why an ADOPTED view's drop is honestly irreversible rather than
// wrong.
//
// So this is the reachability proof the call graph could only assert: three deploys
// (create the view, rename its source, drop the view), one rollback, and the SERVER
// asked what the restored body reads. Before the fold followed a table rename into a
// view body, this rollback failed with `relation "<schema>.<old>" does not exist`.
//
// The three migrations are SEPARATE deploys on purpose. Within one deploy the pending
// ops re-fold from the base snapshot together, and the rollback path is never the one
// that has to recover the body.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`. The arm announces its skip rather than reporting
// the same count either way.
import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t, view } from "zero-migrate";
import { apply, rollback, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_view_inverse_after_rename";
const SOURCE = "vinv_items";
const RENAMED = "vinv_members";
const VIEW = "vinv_active";

type NamedMigration = MigrationModule & { readonly name: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function pgIdent(value: string): string {
  return `"${value.replaceAll('"', '""')}"`;
}

/**
 * The shared charter plus `schema.rename`.
 *
 * Written here rather than added to `noInjectPolicy`, for the reason `policy.ts`
 * already gives about its other narrow charters: every other host arm runs without
 * the rename grant, and widening the shared one to serve this file would loosen all
 * of them at once.
 */
function renamePolicy(projectSchema: string): string {
  const scope = `{ include = [${JSON.stringify(projectSchema)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}

# The grant this charter exists for.
[[grant]]
key = "schema.rename"
value = true
scope = ${scope}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

/**
 * The view names its source in a QUALIFIER as well as in the FROM clause.
 *
 * A body of `SELECT id FROM vinv_items` alone cannot tell a rewrite that follows the
 * rename into qualifiers from one that only moves the FROM relation, and PostgreSQL
 * moves both: `pg_get_viewdef` deparses `vinv_members.id` the instant the rename
 * commits. The restored body is compared against the server's own deparse of the
 * pre-drop one, so the qualifier is part of what has to match.
 */
const createTableAndView: NamedMigration = {
  name: "vinv_create",
  default: {
    schema() {
      table(SOURCE).create({
        columns: { id: t.int().notNull(), label: t.string().notNull() },
        primaryKey: ["id"],
      });
      view(VIEW).create({
        as: (q) =>
          q.from(SOURCE).select([
            { kind: "colRef", table: SOURCE, name: "id" },
            { kind: "colRef", table: SOURCE, name: "label" },
          ]),
      });
    },
  },
} as NamedMigration;

/** PostgreSQL follows this into the view's stored body by OID. The fold has to too. */
const renameTheSource: NamedMigration = {
  name: "vinv_rename",
  default: {
    schema() {
      table(SOURCE).rename({ to: RENAMED });
    },
  },
} as NamedMigration;

/** Unguarded: an `ifExists` drop is deliberately irreversible and asks another question. */
const dropTheView: NamedMigration = {
  name: "vinv_drop",
  default: {
    schema() {
      view(VIEW).drop();
    },
  },
} as NamedMigration;

const ALL = [createTableAndView, renameTheSource, dropTheView];

async function withSchema<T>(
  admin: { query: (text: string, values?: unknown[]) => Promise<{ rows: unknown[] }> },
  body: (projectSchema: string) => Promise<T>,
): Promise<T> {
  const projectSchema = uniqueNamespace("vinv");
  try {
    await admin.query(`CREATE SCHEMA ${pgIdent(projectSchema)}`);
    return await body(projectSchema);
  } finally {
    await admin
      .query(
        `DROP SCHEMA IF EXISTS ${pgIdent(projectSchema)} CASCADE;
         DROP SCHEMA IF EXISTS ${pgIdent(`${projectSchema}_migrations`)} CASCADE`,
      )
      .catch(() => {});
  }
}

test("a table rename reaches the view body a rollback re-creates from", async (ctx) => {
  const admin = await connectLivePg(ctx);
  if (!admin) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  // `connectLivePg` registers no teardown, so the client has to be closed here. An
  // open one keeps the file's event loop alive and the test process never exits -
  // which looks exactly like a hung assertion rather than a leaked handle.
  try {
    await withSchema(admin as never, async (projectSchema) => {
      const base = {
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        policy: [renamePolicy(projectSchema)],
        approved: true,
        appliedBy: "view-inverse-after-table-rename",
      };
      const registry = { [SOURCE]: OWNER_APP, [RENAMED]: OWNER_APP };

      /** The live body as `pg_get_viewdef` reports it, or null when the view is gone. */
      const liveBody = async (): Promise<string | null> => {
        const result = await admin.query(
          `SELECT pg_get_viewdef(c.oid, true) AS body
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'v'`,
          [projectSchema, VIEW],
        );
        const rows = result.rows as Array<{ body: string }>;
        return rows.length === 0 ? null : rows[0]!.body;
      };

      for (let index = 0; index < ALL.length; index += 1) {
        const priors = ALL.slice(0, index);
        await apply({
          ...base,
          migration: ALL[index]!,
          priorMigrations: priors,
          priorNameFallbacks: priors.map((prior) => prior.name),
          registry,
          nameFallback: ALL[index]!.name,
        });

        if (index === 1) {
          // After the rename and before the drop: the premise. Without the server
          // moving BOTH the relation and the qualifier there is nothing here that a
          // FROM-only rewrite would fail.
          const renamed = await liveBody();
          assert.ok(renamed, "the view must survive the rename of its source");
          assert.match(renamed, new RegExp(RENAMED), "PostgreSQL re-renders the body under the new name");
          assert.doesNotMatch(
            renamed,
            new RegExp(`${SOURCE}\\.`),
            "PostgreSQL moves the QUALIFIER too, which is what makes this case able to " +
              "distinguish a qualifier-following rewrite from a FROM-only one",
          );
        }
      }

      const beforeRollback = await liveBody();
      assert.equal(beforeRollback, null, "the drop must actually remove the view");

      const outcome = await rollback({
        migrations: ALL,
        nameFallbacks: ALL.map((migration) => migration.name),
        ownerApp: OWNER_APP,
        projectSchema,
        driver,
        registry,
        policy: [renamePolicy(projectSchema)],
        target: { kind: "steps", steps: 1 },
        approved: true,
        backupAcknowledged: true,
        appliedBy: "view-inverse-after-table-rename",
      });
      assert.deepEqual(
        outcome.skippedIrreversible,
        [],
        "an unguarded dropView whose creating migration is in the replayed history has " +
          "a recoverable body, so it must not be skipped as irreversible",
      );

      const restored = await liveBody();
      assert.ok(restored, "rolling back the drop must put the view back");
      assert.match(
        restored,
        new RegExp(RENAMED),
        "the restored body must read the table under its CURRENT name - a body still " +
          "naming the pre-rename table is SQL PostgreSQL refuses, which is how this " +
          "arm failed before the fold followed a rename into a view body",
      );
    });
  } finally {
    await admin.end().catch(() => {});
  }
});
