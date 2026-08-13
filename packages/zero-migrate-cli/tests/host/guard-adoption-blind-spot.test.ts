// What guarded adoption compares, and the one class it does not.
//
// A guarded `createTable ifNotExists` over an EXISTING table exists to prove the
// live table matches the declared shape before adopting it. It compares a lot,
// and every one of those comparisons is pinned in the first test here.
//
// It does NOT report a declared constraint that is ABSENT from the live table -
// of any kind. `drift.rs` carries two passes: an ATTRIBUTE pass that compares
// objects present under the same name on both sides, and a MISSING/UNEXPECTED
// pass (`diff_named(..., "constraint ", ...)`, `diff_indexes(...)`) that reports
// objects present on only one side. The guard runs the attribute pass only.
//
// So a table missing the declared PRIMARY KEY, or missing a declared UNIQUE, is
// adopted silently and the migration is journaled as applied. The consequences
// are real: no uniqueness enforcement on a column every later migration treats as
// keyed, and a collision with the backfill contract, which requires "an exact
// ordered, non-null primary or unique candidate-key tuple" - a later backfill on
// that table either fails confusingly or builds its cursor guarantees on a key
// that is not unique.
//
// THE SECOND TEST IS WRITTEN TO FAIL WHEN THIS IMPROVES, not to bless it. It
// records the current answer so the change is loud and lands with a decision
// attached, because the decision is not obvious: `ifNotExists` is exactly how
// people point at tables they did not create, and making adoption strict changes
// what it accepts for every existing user. The fix itself is small - the
// missing/unexpected pass already exists, is already shared with the migration
// differ, and is already exercised by the structural-drift path.
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

const OWNER_APP = "app_guard_blind_spot";

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
`;
}

/** DECLARED: `items(id int NOT NULL, name varchar(255) NOT NULL)`,
 *  `PRIMARY KEY (id)`, and a named `UNIQUE (name)`. */
const declared = {
  name: "adopt_items",
  default: {
    up() {
      table("items").create({
        columns: { id: t.int().notNull(), name: t.string({ length: 255 }).notNull() },
        primaryKey: ["id"],
        uniques: [{ name: "items_name_key", columns: ["name"] }],
        ifNotExists: true,
      });
    },
  },
} as MigrationModule & { name: string };

const FULL_SHAPE =
  `id integer NOT NULL, name varchar(255) NOT NULL, ` +
  `PRIMARY KEY (id), CONSTRAINT items_name_key UNIQUE (name)`;

test("guarded adoption refuses every column-shape difference it can see", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  const adoptOver = async (liveDdl: string): Promise<void> => {
    const schema = uniqueNamespace("guard_cols");
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      await client.query(`CREATE TABLE "${schema}".items (${liveDdl})`);
      await apply({
        migration: declared,
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: {},
        policy: [charter(schema)],
        approved: true,
        appliedBy: "guard-adoption-blind-spot",
        nameFallback: declared.name,
      });
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
    // The control: the exact declared shape adopts cleanly. Without it every
    // refusal below also holds for a guard that refuses everything.
    await adoptOver(FULL_SHAPE);

    for (const [label, liveDdl, reason] of [
      [
        "an extra column",
        `id integer NOT NULL, name varchar(255) NOT NULL, extra integer, PRIMARY KEY (id), CONSTRAINT items_name_key UNIQUE (name)`,
        /columns/,
      ],
      [
        "a missing declared column",
        `id integer NOT NULL, PRIMARY KEY (id)`,
        /data_type|columns/,
      ],
      [
        "a nullable column declared NOT NULL",
        `id integer, name varchar(255) NOT NULL, CONSTRAINT items_name_key UNIQUE (name)`,
        /nullable/,
      ],
      [
        "a differing column type",
        `id integer NOT NULL, name integer NOT NULL, PRIMARY KEY (id), CONSTRAINT items_name_key UNIQUE (name)`,
        /data_type/,
      ],
    ] as ReadonlyArray<readonly [string, string, RegExp]>) {
      await assert.rejects(
        adoptOver(liveDdl),
        reason,
        `${label} must be refused, and for its own reason`,
      );
    }
  } finally {
    await client.end().catch(() => {});
  }
});

test("TODAY guarded adoption ignores a declared constraint the live table lacks", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;
  const driver: DriverConfig = { kind: "postgres", url: pgUrl() };

  /** Adopt over `liveDdl` and report the live constraint kinds afterwards. */
  const adoptAndRead = async (liveDdl: string): Promise<string[]> => {
    const schema = uniqueNamespace("guard_absent");
    try {
      await client.query(`CREATE SCHEMA "${schema}"`);
      await client.query(`CREATE TABLE "${schema}".items (${liveDdl})`);
      await apply({
        migration: declared,
        ownerApp: OWNER_APP,
        projectSchema: schema,
        driver,
        registry: {},
        policy: [charter(schema)],
        approved: true,
        appliedBy: "guard-adoption-blind-spot",
        nameFallback: declared.name,
      });
      const { rows } = await client.query(
        `SELECT contype FROM pg_constraint
          WHERE conrelid = ($1 || '.items')::regclass AND contype IN ('p', 'u')
          ORDER BY contype`,
        [`"${schema}"`],
      );
      return rows.map((row) => row.contype as string);
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
    // Each of these declares a PRIMARY KEY and a UNIQUE, and adopts a table that
    // is missing one of them. When the guard learns to consult the
    // missing/unexpected pass, these three calls start throwing and this test
    // fails - which is the point. Replace it then with the refusal assertions,
    // and record what was decided about adoption's contract.
    assert.deepEqual(
      await adoptAndRead(`id integer NOT NULL, name varchar(255) NOT NULL, PRIMARY KEY (id)`),
      ["p"],
      "TODAY: a table missing the declared UNIQUE is adopted; when this refuses, update the file",
    );

    assert.deepEqual(
      await adoptAndRead(
        `id integer NOT NULL, name varchar(255) NOT NULL, CONSTRAINT items_name_key UNIQUE (name)`,
      ),
      ["u"],
      "TODAY: a table missing the declared PRIMARY KEY is adopted; when this refuses, update the file",
    );

    assert.deepEqual(
      await adoptAndRead(`id integer NOT NULL, name varchar(255) NOT NULL`),
      [],
      "TODAY: a table with NEITHER declared constraint is adopted; when this refuses, update the file",
    );
  } finally {
    await client.end().catch(() => {});
  }
});
