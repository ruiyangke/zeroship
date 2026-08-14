// Who is allowed to authorise a destructive operation.
//
// `safety.destructive_ops` is a Global knob that defaults to `forbid`, and
// `docs/policy.md` says grants "become tighter as they move downward through
// admission". Together those make one privilege-escalation question concrete:
// can a LATER layer - a draft an application team writes - grant itself
// permission to drop things that the operator's root charter never allowed?
//
// It cannot, and this file is the measurement. `policy-layering.test.ts` covers
// the same boundary for `[[inject]]`; this covers it for the safety knob, which
// is the half where the failure is an escalation rather than an omission.
//
// Five arms, each a different outcome and a different code path. The refusals are
// not interchangeable and the test asserts each one's own words:
//
//   guard denial          the statement was rendered and the guard refused it
//   GrantExceedsCharter   the draft never loaded - refused above the guard
//   approval refusal      the plan was well-formed and simply unapproved
//
// The baseline arm - root allows, drop succeeds - is what stops the other four
// from passing on a build where destructive operations never work at all.
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

const OWNER_APP = "app_destructive_layering";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function grants(schema: string): string {
  const scope = `{ include = [${JSON.stringify(schema)}] }`;
  return `[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}
`;
}

const ALLOW_DESTRUCTIVE = `
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;

const FORBID_DESTRUCTIVE = `
[[grant]]
key = "safety.destructive_ops"
value = "forbid"
scope = "all"
`;

const created = {
  name: "create_items",
  default: {
    schema() {
      table("items").create({
        columns: { id: t.int().notNull(), doomed: t.int() },
        primaryKey: ["id"],
      });
    },
  },
} as MigrationModule & { name: string };

const dropped = {
  name: "drop_col",
  default: {
    schema() {
      table("items").column("doomed").drop();
    },
  },
} as MigrationModule & { name: string };

test("only the root charter can authorise a destructive operation", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  /** Create the table under a permissive charter, then attempt the drop under
   *  `layers`. The setup is deliberately separate so every arm differs ONLY in
   *  the policy governing the destructive step. */
  const attemptDrop = async (
    layers: readonly string[],
    approved: boolean,
    schema: string,
  ): Promise<string[]> => {
    await apply({
      migration: created,
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver,
      registry: {},
      policy: [`policy_version = 1\n\n${grants(schema)}${ALLOW_DESTRUCTIVE}`],
      approved: true,
      appliedBy: "destructive-ops-layering",
      nameFallback: created.name,
    });
    await apply({
      migration: dropped,
      priorMigrations: [created],
      priorNameFallbacks: [created.name],
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver,
      registry: { items: OWNER_APP },
      policy: layers,
      approved,
      appliedBy: "destructive-ops-layering",
      nameFallback: dropped.name,
    });
    const { rows } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'items' ORDER BY column_name`,
      [schema],
    );
    return rows.map((row) => row.column_name as string);
  };

  const withSchema = async <T>(run: (schema: string) => Promise<T>): Promise<T> => {
    const schema = uniqueNamespace("destr_layer");
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      return await run(schema);
    } finally {
      await client
        .query(
          `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
           DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
        )
        .catch(() => {});
    }
  };

  try {
    // 1. Baseline. The root allows it, so the column really goes. Without this,
    //    every refusal below also holds for a build where drops never work.
    await withSchema(async (schema) => {
      assert.deepEqual(
        await attemptDrop([`policy_version = 1\n\n${grants(schema)}${ALLOW_DESTRUCTIVE}`], true, schema),
        ["id"],
        "a root charter that allows destructive ops must let the drop through",
      );
    });

    // 2. Default-deny: a root that says nothing forbids it.
    await withSchema(async (schema) => {
      await assert.rejects(
        attemptDrop([`policy_version = 1\n\n${grants(schema)}`], true, schema),
        /DATA_SECURITY_DESTRUCTIVE_OPS_FORBID/,
        "the knob must default to forbid when the root is silent",
      );
    });

    // 3. THE ESCALATION ARM. The root is silent, so destructive ops are forbidden;
    //    a draft then grants itself permission. It must not load at all - refused
    //    ABOVE the guard, because a draft that could reach the guard with a
    //    widened grant would already have escalated.
    await withSchema(async (schema) => {
      await assert.rejects(
        attemptDrop(
          [`policy_version = 1\n\n${grants(schema)}`, `policy_version = 1\n${ALLOW_DESTRUCTIVE}`],
          true,
          schema,
        ),
        /GrantExceedsCharter/,
        "a draft must not be able to grant itself destructive permission",
      );
    });

    // 4. Narrowing downward still works: a draft may take away what the root gave.
    await withSchema(async (schema) => {
      await assert.rejects(
        attemptDrop(
          [
            `policy_version = 1\n\n${grants(schema)}${ALLOW_DESTRUCTIVE}`,
            `policy_version = 1\n${FORBID_DESTRUCTIVE}`,
          ],
          true,
          schema,
        ),
        /DATA_SECURITY_DESTRUCTIVE_OPS_FORBID/,
        "a draft must still be able to forbid what the root permits",
      );
    });

    // 5. Permission is not approval. Even fully permitted, the destructive plan
    //    needs an explicit approval, and the refusal says so in its own terms.
    await withSchema(async (schema) => {
      await assert.rejects(
        attemptDrop([`policy_version = 1\n\n${grants(schema)}${ALLOW_DESTRUCTIVE}`], false, schema),
        /requires approval \(destructive\)/,
        "a permitted destructive plan must still be refused without approval",
      );
    });
  } finally {
    await client.end().catch(() => {});
  }
});
