// Exact numbers survive the full stack, and inexact ones are refused rather than
// rounded.
//
// The two adversarial-value files beside this one are about INJECTION - a value
// that escapes its slot. This is about COERCION - a value that stays in its slot
// and arrives as a different number. That failure is quieter: nothing errors, the
// migration succeeds, and the wrong figure is now the record.
//
// The load-bearing arm is the REFUSAL. A JavaScript number cannot represent every
// signed 64-bit integer, so an author who writes one past 2^53 has already lost
// precision before the engine sees it. The engine refuses it and names the fix
// (`int64(...)`) instead of storing the nearest double - which is the difference
// between a failed deploy and a silently wrong identifier.
//
// `ops.test.ts` covers that refusal at the DSL layer. What it cannot show is that
// an exact `int64` survives authoring, lowering, the addon boundary, the driver
// and PostgreSQL's own parsing - five conversions, any one of which could have
// gone through a double. Reading `9223372036854775807` back as text is the proof
// that none did.
//
// SCALE ROUNDING IS NOT A DEFECT and the arm records it as expected: a
// `numeric(10,2)` column rounds 1.005 to 1.01 because that is what a scale of 2
// means. It is here so the behaviour is written down rather than discovered.
//
// GATE: `connectLivePg` (see `live-db.ts`).

import assert from "node:assert/strict";
import { test } from "node:test";

import { table, t, int64, decimal } from "zero-migrate";
import { apply, type DriverConfig } from "zero-migrate-cli";
import type { MigrationModule } from "zero-migrate/internal/recorder";

import { connectLivePg, pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const OWNER_APP = "app_numeric_precision";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function charter(scopeName: string): string {
  const scope = `{ include = [${JSON.stringify(scopeName)}] }`;
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

test("an exact int64 survives the whole stack, and an unsafe JS number is refused", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const run = async (
    columns: Record<string, unknown>,
    row: Record<string, unknown>,
    read: string,
    schema: string,
  ): Promise<Record<string, unknown> | undefined> => {
    // Schema and data are SEPARATE migrations now: one module may not carry both,
    // so the fixture applies the table first and the row second.
    const applyOne = (migration: MigrationModule, nameFallback: string) =>
      apply({
        migration,
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: { items: OWNER_APP },
        policy: [charter(schema)],
        approved: true,
        appliedBy: "numeric-precision-boundary",
        nameFallback,
      });

    await applyOne(
      {
        default: {
          schema() {
            table("items").create({
              columns: { id: t.int().notNull(), ...(columns as never) },
              primaryKey: ["id"],
            });
          },
        },
      } as MigrationModule,
      "create_it",
    );
    await applyOne(
      {
        default: {
          data() {
            table("items").insert({ rows: { id: 1, ...row } });
          },
          // Exactly reversible: the forward inserts one known row, so the
          // inverse deletes exactly that row.
          inverse() {
            table("items").delete({ where: (col) => col("id").eq(1) });
          },
        },
      } as MigrationModule,
      "insert_it",
    );
    const { rows } = await client.query(read.replaceAll("@S", `"${schema}"`));
    return rows[0];
  };

  const withSchema = async <T>(body: (schema: string) => Promise<T>): Promise<T> => {
    const schema = uniqueNamespace("numprec");
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      return await body(schema);
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
    // The i64 maximum, read back AS TEXT so the assertion cannot itself go
    // through a double on the way out.
    await withSchema(async (schema) => {
      const row = await run(
        { n: t.bigInt() },
        { n: int64("9223372036854775807") },
        `SELECT n::text AS n FROM @S.items WHERE id = 1`,
        schema,
      );
      assert.equal(
        row?.n,
        "9223372036854775807",
        "the i64 maximum must survive authoring, lowering, the addon, the driver and PostgreSQL",
      );
    });

    // The refusal. 9007199254740993 is 2^53 + 1, which JavaScript cannot hold -
    // the literal in this file is ALREADY 9007199254740992 by the time it is a
    // number, so storing it would record a figure nobody wrote.
    await withSchema(async (schema) => {
      await assert.rejects(
        run(
          { n: t.bigInt() },
          { n: 9007199254740993 },
          `SELECT n::text AS n FROM @S.items WHERE id = 1`,
          schema,
        ),
        /safe integer|int64/i,
        "a JS number past 2^53 must be refused, and the message must name int64()",
      );
    });

    // Declared scale rounds, which is what a scale of 2 means. Recorded, not
    // treated as a defect - and asserted from an exact decimal() so the rounding
    // is the column's and not a float's.
    await withSchema(async (schema) => {
      const row = await run(
        { amount: t.numeric({ precision: 10, scale: 2 }) },
        { amount: decimal("1.005") },
        `SELECT amount::text AS amount FROM @S.items WHERE id = 1`,
        schema,
      );
      assert.equal(row?.amount, "1.01", "numeric(10,2) rounds to its declared scale");
    });
  } finally {
    await client.end().catch(() => {});
  }
});
