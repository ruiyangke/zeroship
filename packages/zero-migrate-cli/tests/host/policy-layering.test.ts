// What a later policy layer can and cannot do to the root charter.
//
// `docs/policy.md`: "Grants become tighter as they move downward through
// admission. Requirements and injections accumulate; an untrusted draft cannot
// remove them."
//
// That is the security property the whole layered-policy design exists for. The
// root charter is operator-owned; later layers are project drafts an application
// team writes. If a draft could strip the root's mandatory `[[inject]]`, every
// table it creates would quietly lack the column the operator mandated - and
// there would be no error, no warning, and nothing in the migration to review.
// The tables would simply be missing it.
//
// `inject-preview-parity.test.ts` covers a single-layer charter: that an inject
// lands and that the preview shows it. Nothing covered the LAYERED case, which is
// the one the security claim is about.
//
// Six arms, each with a distinct outcome. That matters: a layering model that
// refused everything from a draft would satisfy the three refusal arms while
// making drafts useless, so two arms assert that a draft's legitimate
// contributions DO take effect.
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

const OWNER_APP = "app_policy_layering";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** The operator-owned root: two grants and one MANDATORY injected column. */
function rootCharter(schema: string): string {
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[inject]]
scope = { include = [${JSON.stringify(`${schema}.*`)}] }
mandatory = true
columns = [
  { name = "created_at", type = "timestamptz", nullable = false },
]
`;
}

const migration = {
  name: "create_items",
  default: {
    up() {
      table("items").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
    },
  },
} as MigrationModule & { name: string };

test("a later policy layer cannot remove, mandate, widen, or contradict the root", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  /** Apply `migration` under `layers`, returning the live column list. */
  const applyUnder = async (layers: readonly string[], schema: string): Promise<string[]> => {
    await apply({
      migration,
      ownerApp: OWNER_APP,
      projectSchema: schema,
      driver,
      registry: {},
      policy: layers,
      approved: true,
      appliedBy: "policy-layering",
      nameFallback: migration.name,
    });
    const { rows } = await client.query(
      `SELECT column_name FROM information_schema.columns
        WHERE table_schema = $1 AND table_name = 'items'
        ORDER BY ordinal_position`,
      [schema],
    );
    return rows.map((row) => row.column_name as string);
  };

  const withSchema = async <T>(run: (schema: string) => Promise<T>): Promise<T> => {
    const schema = uniqueNamespace("pol_layer");
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
    // 1. Baseline: the root alone injects its column.
    await withSchema(async (schema) => {
      assert.deepEqual(
        await applyUnder([rootCharter(schema)], schema),
        ["created_at", "id"],
        "the root charter's mandatory inject must land",
      );
    });

    // 2. THE SECURITY PROPERTY. A draft that simply does not mention the inject
    //    does not thereby remove it. This is the arm the claim is about: silence
    //    from the draft must not be consent to drop the operator's column.
    await withSchema(async (schema) => {
      const draft = `policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }
`;
      assert.deepEqual(
        await applyUnder([rootCharter(schema), draft], schema),
        ["created_at", "id"],
        "a draft that omits the inject must not remove it",
      );
    });

    // 3. A draft's own NON-mandatory inject accumulates. Without this arm the
    //    refusals below are equally consistent with a model that ignores drafts
    //    entirely, which would be a different (and useless) design.
    await withSchema(async (schema) => {
      const draft = `policy_version = 1

[[inject]]
scope = { include = [${JSON.stringify(`${schema}.*`)}] }
columns = [ { name = "updated_at", type = "timestamptz", nullable = false } ]
`;
      assert.deepEqual(
        await applyUnder([rootCharter(schema), draft], schema),
        ["created_at", "id", "updated_at"],
        "a draft's non-mandatory inject must accumulate alongside the root's",
      );
    });

    // 4. Only the root may MANDATE. A draft that tries is refused at load.
    await withSchema(async (schema) => {
      const draft = `policy_version = 1

[[inject]]
scope = { include = [${JSON.stringify(`${schema}.*`)}] }
mandatory = true
columns = [ { name = "updated_at", type = "timestamptz", nullable = false } ]
`;
      await assert.rejects(
        applyUnder([rootCharter(schema), draft], schema),
        /MandatoryInjectOnNonRootLayer/,
        "a non-root layer must not be able to mandate an injection",
      );
    });

    // 5. Grants only narrow downward.
    await withSchema(async (schema) => {
      const draft = `policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"
`;
      await assert.rejects(
        applyUnder([rootCharter(schema), draft], schema),
        /UncoveredRegionNotRepresentable|schema\.create_table/,
        "a draft must not widen a grant beyond the root's scope",
      );
    });

    // 6. A draft cannot neutralise an inject indirectly either, by forbidding the
    //    column the root contributes. Without this arm, arm 2's guarantee has an
    //    obvious back door.
    await withSchema(async (schema) => {
      const draft = `policy_version = 1

[[validate]]
scope = { include = [${JSON.stringify(`${schema}.*`)}] }
predicate = { kind = "forbidden_columns", names = ["created_at"] }
`;
      await assert.rejects(
        applyUnder([rootCharter(schema), draft], schema),
        /DraftValidateContradictsCharterInject/,
        "a draft must not forbid a column the root charter injects",
      );
    });

    // 7. And the same contradiction inside ONE document is refused at load, so it
    //    cannot arrive from either direction.
    await withSchema(async (schema) => {
      const selfContradictory = `${rootCharter(schema)}
[[validate]]
scope = { include = [${JSON.stringify(`${schema}.*`)}] }
predicate = { kind = "forbidden_columns", names = ["created_at"] }
`;
      await assert.rejects(
        applyUnder([selfContradictory], schema),
        /SelfContradictoryInjectValidate/,
        "a charter forbidding its own injected column must be refused when it loads",
      );
    });
  } finally {
    await client.end().catch(() => {});
  }
});
