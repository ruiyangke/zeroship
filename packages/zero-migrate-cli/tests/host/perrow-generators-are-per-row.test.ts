// A `perRow` generator produces a DISTINCT value for every backfilled row.
//
// The whole point of `perRow.uuidV4()` over a sampled literal is that the recorder
// stores an INTENT, not a value -- `ops.test.ts` pins that the IR carries
// `{ perRow: "uuidV4" }` and never a UUID literal. What nothing checked is the far
// end: that the apply engine honours the intent by generating once PER ROW.
//
// The failure mode is silent and total. If the generator were evaluated once and
// the result reused, every row would receive the SAME id, the backfill would
// report success, and the corruption would surface later as a unique-constraint
// violation or, worse, as rows that silently alias each other. Nothing about the
// migration would look wrong. That is the F619 shape -- a data path whose bug does
// not fail the deploy -- so the assertion is DISTINCTNESS in the database, not the
// IR shape and not the exit code.
//
// THREE PROPERTIES, each of which a plausible implementation could get wrong
// separately:
//
//   distinct across rows  the generator ran per row rather than once
//   independent columns   `u7` and `u7b` share ONE generator intent (the same
//                         `perRow.uuidV7()` value was assigned to both). They must
//                         still receive different values, because the intent is a
//                         generator and not a sample. An implementation that
//                         memoised per intent would tie them together.
//   format honoured       the TypeID prefix survives into the stored value
//
// The value-format contracts are load-bearing rather than decorative: the engine
// REFUSES `perRow.typeId({prefix})` into a generic `t.text()` column, because
// generic text carries no value-format contract to validate against. The columns
// here use `ids.typeId(...)` and `ids.ulid()` for that reason.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { pgUrl } from "./live-db.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const PG_URL = process.env.ZERO_MIGRATE_TEST_PG_URL;
const OWNER_APP = "app_perrow";
const TABLE = "pr_rows";
/** Enough rows that a per-row generator and a once-generated value cannot be
 *  confused by chance, and few enough to stay fast. */
const ROWS = 6;

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(): string {
  const work = mkdtempSync(join(HERE, "perrow-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  const seed = Array.from({ length: ROWS }, (_, i) => `{ id: ${i + 1} }`).join(", ");
  writeFileSync(
    join(work, "migrations", "20260101000000_a.ts"),
    `import { table, t, ids } from "zero-migrate";
export const name = "a";
export default {
  up() {
    table("${TABLE}").create({
      columns: {
        id: t.int().notNull(),
        u4: t.uuid(),
        u7: t.uuid(),
        u7b: t.uuid(),
        tid: ids.typeId({ prefix: "order" }),
        ul: ids.ulid(),
      },
      primaryKey: ["id"],
    });
    table("${TABLE}").insert({ rows: [${seed}] });
  },
};
`,
  );
  writeFileSync(
    join(work, "migrations", "20260102000000_b.ts"),
    `import { table, perRow } from "zero-migrate";
export const name = "b";
export default {
  data() {
    // Deliberately assigned to TWO columns: one intent, two destinations.
    const reused = perRow.uuidV7();
    table("${TABLE}").backfill({
      set: {
        u4: perRow.uuidV4(),
        u7: reused,
        u7b: reused,
        tid: perRow.typeId({ prefix: "order" }),
        ul: perRow.ulid(),
      },
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
    });
  },
  irreversible: "overwrites u4, u7, u7b, tid, and ul for existing ${TABLE} rows; prior values are not recorded",
};
`,
  );
  return work;
}

test("every perRow generator yields a distinct value per backfilled row", async (ctx) => {
  if (!PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("perrow_pg");
  const work = project();
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = spawnSync(
      process.execPath,
      [
        "--import", "tsx", CLI_BIN, "apply", "--approve",
        "--dir", join(work, "migrations"),
        "--database-url", pgUrl(),
        "--policy", join(work, "policy.toml"),
        "--registry", join(work, "registry.json"),
        "--schema", namespace,
        "--owner-app", OWNER_APP,
      ],
      {
        cwd: work,
        encoding: "utf8",
        env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
      },
    );
    assert.equal(
      applied.status,
      0,
      `the backfill must apply; ${`${applied.stdout}\n${applied.stderr}`.trim()}`,
    );

    const { rows } = await client.query(
      `SELECT id, u4, u7, u7b, tid, ul FROM "${namespace}"."${TABLE}" ORDER BY id`,
    );
    assert.equal(rows.length, ROWS, "every seeded row must still be present");

    for (const column of ["u4", "u7", "u7b", "tid", "ul"] as const) {
      const values = rows.map((row: Record<string, unknown>) => String(row[column]));
      assert.equal(
        new Set(values).size,
        ROWS,
        `${column}: a generator evaluated once and reused would give every row the ` +
          `same value, and the backfill would still report success; got ${JSON.stringify(values)}`,
      );
      assert.ok(
        values.every((value) => value !== "null" && value.length > 0),
        `${column}: every row must actually have been written`,
      );
    }

    // One intent, two destinations. Memoising per intent would tie these together.
    for (const row of rows as Array<Record<string, unknown>>) {
      assert.notEqual(
        String(row.u7),
        String(row.u7b),
        "two columns sharing one generator intent must still receive independent " +
          "values -- the intent is a generator, not a sampled value",
      );
    }

    // The declared value format survives into the stored value.
    for (const row of rows as Array<Record<string, unknown>>) {
      assert.match(
        String(row.tid),
        /^order_[0-9a-z]+$/,
        "the TypeID prefix declared on the column must be honoured by the generator",
      );
      assert.match(String(row.ul), /^[0-9A-Z]{26}$/, "a ULID is 26 uppercase base32 chars");
    }
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${namespace}" CASCADE;
         DROP SCHEMA IF EXISTS "${namespace}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
