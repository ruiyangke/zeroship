// `history --json` promises exact integers, and only a huge one can prove it.
//
// docs/cli.md: "`--json` emits the structured reply and serializes exact integer
// sequence values without precision loss."
//
// `event_seq` is a `BIGINT GENERATED ALWAYS AS IDENTITY`, so a real journal
// reaches large values only after more deploys than any test can perform. Every
// existing assertion therefore runs in the range where a `Number()` conversion is
// invisible: 1, 2, 3 survive a round trip through a double unchanged. A build
// that had lost the bigint somewhere would pass all of them.
//
// So this restarts the identity at 2^53 + 1 - the smallest integer a JavaScript
// double cannot represent, where `Number(9007199254740993) === 9007199254740992`
// - and requires the JSON to carry it exactly.
//
// A STRING IS THE CORRECT ANSWER, and the test asserts that rather than a number.
// JSON has no integer type; a bare `9007199254740993` in the document would be
// read back as 9007199254740992 by any parser using doubles, so emitting it as a
// number would break the promise no matter what the engine did internally. The
// type assertion is therefore part of the contract, not an implementation detail.
//
// The Node API is a different surface with a different answer: `e2e-pg.test.ts`
// and `driver-pg.test.ts` assert `typeof eventSeq === "bigint"` there, because
// napi6 can carry one. Neither says anything about the CLI's JSON.
//
// GATE: `ZERO_MIGRATE_TEST_PG_URL`.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const ABI = process.platform === "linux" ? "-gnu" : "";
const ADDON_PATH = resolve(
  HERE,
  `../../../../crates/zero-migrate-node/zero-migrate-node.${process.platform}-${process.arch}${ABI}.node`,
);

const OWNER_APP = "app_json_bigint";
/** 2^53 + 1: the smallest integer a JavaScript double cannot represent. */
const BEYOND_DOUBLE = "9007199254740993";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function project(schema: string): string {
  const work = mkdtempSync(join(HERE, "jsonbig-"));
  mkdirSync(join(work, "migrations"));
  writeFileSync(
    join(work, "policy.toml"),
    `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = [${JSON.stringify(schema)}] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = [${JSON.stringify(schema)}] }
`,
  );
  return work;
}

function writeMigration(work: string, index: number, table: string): void {
  writeFileSync(
    join(work, "migrations", `2026010${index}000000_m${index}.ts`),
    `import { table, t } from "zero-migrate";
export const name = "m${index}";
export default {
  schema() {
    table(${JSON.stringify(table)}).create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
  },
};
`,
  );
}

function run(work: string, schema: string, argv: string[]): { code: number | null; out: string; err: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--database-url", pgUrl(),
      "--policy", join(work, "policy.toml"),
      "--schema", schema,
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    out: result.stdout ?? "",
    err: (result.stderr ?? "").replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("history --json carries an event_seq beyond 2^53 exactly", async (ctx) => {
  const client = await connectLivePg(ctx);
  if (!client) return;

  const schema = uniqueNamespace("jsonbig");
  const meta = `${schema}_migrations`;
  const work = project(schema);
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    writeMigration(work, 1, "t1");
    assert.equal(run(work, schema, ["apply", "--approve"]).code, 0, "the journal must exist");

    // Push the identity past what a double can hold, then journal one more event.
    await client.query(
      `ALTER TABLE "${meta}".schema_migrations
         ALTER COLUMN event_seq RESTART WITH ${BEYOND_DOUBLE}`,
    );
    writeMigration(work, 2, "t2");
    const applied = run(work, schema, ["apply", "--approve"]);
    assert.equal(applied.code, 0, `the second apply must succeed; ${applied.err}`);

    // The premise, read as text so this assertion cannot itself lose precision.
    const { rows: stored } = await client.query(
      `SELECT event_seq::text AS seq FROM "${meta}".schema_migrations
        ORDER BY event_seq DESC LIMIT 1`,
    );
    assert.equal(
      stored[0].seq,
      BEYOND_DOUBLE,
      "the journal must really hold the huge sequence value",
    );

    const reported = run(work, schema, ["history", "--json"]);
    assert.equal(reported.code, 0, `history must succeed; ${reported.err}`);
    const events = (JSON.parse(reported.out) as { events: Array<{ eventSeq: unknown }> }).events;
    const last = events[events.length - 1];

    // A string, deliberately: JSON has no integer type, and a bare
    // 9007199254740993 in the document would be read back as ...992 by any
    // double-based parser. The type is part of the promise.
    assert.equal(
      typeof last.eventSeq,
      "string",
      "an exact integer must cross JSON as a string, not a number",
    );
    assert.equal(
      last.eventSeq,
      BEYOND_DOUBLE,
      "and it must be the value the journal holds, digit for digit",
    );

    // The value really is past the double boundary - so a Number() anywhere in
    // the path would have changed it, and this test would have caught it.
    assert.notEqual(
      String(Number(BEYOND_DOUBLE)),
      BEYOND_DOUBLE,
      "the fixture value must be one a double cannot represent, or it proves nothing",
    );

    // And the raw document, not just the parsed object: a quoted digit string.
    assert.match(
      reported.out,
      new RegExp(`"eventSeq":\\s*"${BEYOND_DOUBLE}"`),
      "the emitted document must quote the value",
    );
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${meta}" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
});
