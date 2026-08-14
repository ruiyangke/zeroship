// Migrations are confined to the project schema, and only the root charter can
// widen that.
//
// This completes the layered-policy sweep. `policy-layering.test.ts` covers
// `[[inject]]`, where a broken boundary means a mandated column quietly goes
// missing. `destructive-ops-layering.test.ts` covers the safety knob, where a
// broken boundary means an escalation. This covers the RESOURCE boundary: which
// schemas a deploy can touch at all.
//
// The failure here would be the widest of the three. A migration that reached
// outside its project schema could create, alter or drop objects belonging to a
// different application entirely - and the deploy that did it would look
// completely ordinary in review, because the schema name is one option on one
// call.
//
// Four arms. THE THIRD IS THE ONE THAT MAKES THE OTHERS MEAN SOMETHING: with the
// other schema granted in the ROOT charter, the same migration applies and the
// table really lands outside. Without it, "the escape was refused" is equally
// consistent with `schema:` being broken, or with the second schema not existing,
// and the file would be pinning an accident.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_schema_confinement";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** Grants confined to `schemas`. */
function charter(...schemas: string[]): string {
  const scope = `{ include = [${schemas.map((s) => JSON.stringify(s)).join(", ")}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}
`;
}

test("a migration cannot reach outside its project schema unless the root charter says so", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  /** Apply a migration creating `items`, optionally qualified into `schema`. */
  const applyCreating = (
    into: string | undefined,
    projectSchema: string,
    policy: readonly string[],
  ) =>
    apply({
      migration: {
        name: "create_items",
        default: {
          schema() {
            const handle = into ? table("items", { schema: into }) : table("items");
            handle.create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
          },
        },
      } as MigrationModule,
      ownerApp: OWNER_APP,
      projectSchema,
      driver,
      registry: {},
      policy,
      approved: true,
      appliedBy: "schema-confinement",
      nameFallback: "create_items",
    });

  const tablesIn = async (schema: string): Promise<string[]> => {
    const { rows } = await client.query(
      `SELECT table_name FROM information_schema.tables WHERE table_schema = $1`,
      [schema],
    );
    return rows.map((row) => row.table_name as string);
  };

  const withSchemas = async <T>(run: (project: string, other: string) => Promise<T>): Promise<T> => {
    const project = uniqueNamespace("confine");
    const other = `${project}_outside`;
    try {
      await client.query(`CREATE SCHEMA "${project}"; CREATE SCHEMA "${other}"`);
      return await run(project, other);
    } finally {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${project}" CASCADE;
           DROP SCHEMA IF EXISTS "${other}" CASCADE;
           DROP SCHEMA IF EXISTS "${project}_migrations" CASCADE`,
        )
        .catch(() => {});
    }
  };

  try {
    // 1. Baseline: unqualified, lands in the project schema.
    await withSchemas(async (project, other) => {
      await applyCreating(undefined, project, [charter(project)]);
      assert.deepEqual(await tablesIn(project), ["items"], "the table lands in the project schema");
      assert.deepEqual(await tablesIn(other), [], "and nowhere else");
    });

    // 2. The confinement. A qualified escape is refused, and the message tells the
    //    author what to do rather than only that it failed.
    await withSchemas(async (project, other) => {
      await assert.rejects(
        applyCreating(other, project, [charter(project)]),
        /CROSS_SCHEMA/,
        "a migration qualified into an ungranted schema must be refused",
      );
      assert.deepEqual(await tablesIn(other), [], "and must leave that schema untouched");
    });

    // 3. THE CONTROL. The SAME migration, with the other schema granted in the
    //    ROOT charter, applies - and the table really lands outside. Confinement
    //    is charter-driven, not a hardcoded refusal of every qualifier.
    await withSchemas(async (project, other) => {
      await applyCreating(other, project, [charter(project, other)]);
      assert.deepEqual(
        await tablesIn(other),
        ["items"],
        "a root charter that grants the other schema must let the migration reach it",
      );
      assert.deepEqual(await tablesIn(project), [], "and it goes only where it was aimed");
    });

    // 4. A draft cannot widen the confinement for itself. Refused at policy load,
    //    the same layer the destructive-ops escalation is caught at.
    await withSchemas(async (project, other) => {
      await assert.rejects(
        applyCreating(other, project, [charter(project), charter(project, other)]),
        /GrantExceedsCharter/,
        "a draft must not be able to add a schema the root charter withholds",
      );
      assert.deepEqual(await tablesIn(other), [], "and must leave that schema untouched");
    });
  } finally {
    await client.end().catch(() => {});
  }
});
