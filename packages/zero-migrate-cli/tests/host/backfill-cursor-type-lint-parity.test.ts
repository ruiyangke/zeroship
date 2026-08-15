// An unsupported backfill cursor type is refused at LINT, not only at apply.
//
// F653. `writing-migrations.md` states the rule plainly: "Floating-point, JSON,
// binary, and geometric types are not supported backfill cursors." The rule
// HOLDS at apply -- measured with the cursor column carrying a UNIQUE constraint
// so its TYPE is the only variable:
//
//   double (unique key)  REFUSED  cursor component "c" has unsupported ordered type
//   int    (unique key)  APPLIED
//   text   (unique key)  APPLIED
//
// The gap is that the SAME migration set passes `lint` with exit 0, even though
// the `createTable` declaring `c` as `double` sits in the same directory and is
// available offline. Lint has the information and did not apply the rule.
//
// `docs/cli.md` cites closing exactly this shape as a fix -- a migration that
// "previously passed lint and failed only at apply" now fails lint. F627 was the
// same shape earlier in this review: green CI, broken deploy.
//
// WHY THIS FILE HAS FOUR ARMS. The reason the finding sat open is a real
// tension: the refusal comes from the planner, which reads the LIVE catalog, and
// the live type need not match what the linted directory declares. So the rule
// is SCOPED rather than skipped:
//
//   - when the linted set CREATES the cursor's table, lint knows the type and
//     must refuse (arm 1), while the same set with a supported type must still
//     pass (arm 2, or a fix that refuses every backfill would satisfy arm 1);
//   - apply must still refuse the unsupported case with its own message (arm 3),
//     so the lint rule is an EARLIER COPY of one verdict rather than a second
//     verdict that can drift from it;
//   - when the table does NOT come from the linted set, lint must NOT guess
//     (arm 4). Without that arm the fix becomes a false-positive machine on
//     every project whose tables predate the directory being linted.
//
// GATES: none for the lint arms; `ZERO_MIGRATE_TEST_PG_URL` for the apply arm.

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

const OWNER_APP = "app_cursor_type";
const TABLE = "ct_rows";

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/** A project whose backfill cursors on `c`, declared with `cursorType`.
 *
 *  `withCreate: false` omits the createTable, so the cursor column's type is NOT
 *  determinable from the linted set -- the case lint must not guess about. */
function project(cursorType: string, withCreate: boolean): string {
  const work = mkdtempSync(join(HERE, "cursortype-"));
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
`,
  );
  writeFileSync(join(work, "registry.json"), JSON.stringify({ [TABLE]: OWNER_APP }));
  if (withCreate) {
    writeFileSync(
      join(work, "migrations", "20260101000000_create.ts"),
      `import { table, t } from "zero-migrate";
export const name = "create";
export default {
  schema() {
    table("${TABLE}").create({
      columns: { c: ${cursorType}.notNull(), val: t.int() },
      primaryKey: ["c"],
    });
  },
};
`,
    );
  }
  writeFileSync(
    join(work, "migrations", "20260101000001_fill.ts"),
    `import { table } from "zero-migrate";
export const name = "fill";
export default {
  data() {
    table("${TABLE}").backfill({
      name: "fill_val",
      set: { val: 1 },
      cursorColumns: ["c"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 2,
    });
  },
  irreversible: "the prior val is not recorded, so the backfill cannot be undone",
};
`,
  );
  return work;
}

function run(work: string, argv: string[]): { code: number | null; text: string } {
  const result = spawnSync(
    process.execPath,
    [
      "--import", "tsx", CLI_BIN, ...argv,
      "--dir", join(work, "migrations"),
      "--policy", join(work, "policy.toml"),
      "--registry", join(work, "registry.json"),
      "--owner-app", OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    // Read the CLI's own status, never `$?` after a pipe -- that reports the last
    // command in the pipeline and has produced false greens on this finding before.
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(/^WARNING.*$/gm, "").trim(),
  };
}

test("lint refuses a cursor whose type the linted set declares unsupported", () => {
  const work = project("t.double()", true);
  try {
    const linted = run(work, ["lint", "--dialect", "postgres"]);
    assert.equal(
      linted.code,
      1,
      `the createTable declaring c as double is IN this directory, so lint has ` +
        `everything it needs. Passing here and failing at apply is green CI over ` +
        `a broken deploy; ${linted.text}`,
    );
    assert.match(
      linted.text,
      /cursor/i,
      `and the refusal must name the cursor, so the author does not have to guess ` +
        `which of the migration's parts is unsupported; ${linted.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("CONTROL: a supported cursor type still lints clean", () => {
  // Without this, a fix that refused every backfill would satisfy the arm above.
  const work = project("t.int()", true);
  try {
    const linted = run(work, ["lint", "--dialect", "postgres"]);
    assert.equal(linted.code, 0, `an int cursor is supported and must pass; ${linted.text}`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("CONTROL: lint does NOT guess when the linted set never declares the column", () => {
  // The table predates this directory, so its live type is unknown offline. A
  // rule that fired here would false-positive on every project whose schema is
  // older than the migrations being linted -- a far worse defect than the gap.
  const work = project("t.double()", false);
  try {
    const linted = run(work, ["lint", "--dialect", "postgres"]);
    assert.equal(
      linted.code,
      0,
      `with no createTable in the set, the cursor's type is not knowable offline ` +
        `and lint must defer rather than guess; ${linted.text}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("apply still refuses the unsupported cursor, so lint is an earlier copy of one verdict", async (ctx) => {
  if (!process.env.ZERO_MIGRATE_TEST_PG_URL) {
    ctx.skip("ZERO_MIGRATE_TEST_PG_URL unset");
    return;
  }
  const pg = await import("pg");
  const client = new pg.Client({ connectionString: pgUrl() });
  await client.connect();
  const namespace = uniqueNamespace("cursortype");
  const work = project("t.double()", true);
  try {
    await client.query(`CREATE SCHEMA "${namespace}"`);
    const applied = run(work, [
      "apply", "--approve",
      "--database-url", pgUrl(),
      "--schema", namespace,
    ]);
    assert.equal(applied.code, 1, `apply must still refuse it; ${applied.text}`);
    assert.match(
      applied.text,
      /unsupported ordered type/,
      `and with the planner's own message. If lint grew a DIFFERENT refusal, the ` +
        `two verdicts could drift and an author would learn two vocabularies for ` +
        `one rule; ${applied.text}`,
    );
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
