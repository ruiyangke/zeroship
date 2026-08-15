// PostgreSQL locking advisories must reach the operator-facing CLI.
//
// The analyzer already identifies the ADD CONSTRAINT emitted for a unique
// addColumn as an ACCESS EXCLUSIVE hazard. These tests keep that information
// visible on both commands an operator reads before deploy: offline
// `lint --explain` and the live `plan` preview. The plain-column controls are
// essential: an unconditional warning would otherwise satisfy the hazard arms.
//
// GATES: none for lint; `ZERO_MIGRATE_TEST_PG_URL` for plan.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { test, type TestContext } from "node:test";
import { fileURLToPath } from "node:url";

import { ADDON_PATH } from "./addon.js";
import { connectLivePg, pgUrl } from "./live-db.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CLI_BIN = resolve(HERE, "../../src/cli-bin.ts");
const OWNER_APP = "app_locksurface";

type CliResult = { code: number | null; text: string };

function uniqueNamespace(prefix: string): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.floor(Math.random() * 1e6).toString(36)}`;
}

function baseProject(): string {
  const work = mkdtempSync(join(HERE, "locksurface-"));
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
  writeFileSync(join(work, "registry.json"), JSON.stringify({ lk_t: OWNER_APP }));
  writeFileSync(
    join(work, "migrations", "20260101000000_create.ts"),
    `import { table, t } from "zero-migrate";
export const name = "create_lk_t";
export default {
  schema() {
    table("lk_t").create({
      columns: { id: t.int().notNull() },
      primaryKey: ["id"],
    });
  },
};
`,
  );
  return work;
}

function addColumnMigration(work: string, unique: boolean): void {
  writeFileSync(
    join(work, "migrations", "20260101000001_add_e.ts"),
    `import { table, t } from "zero-migrate";
export const name = "add_e";
export default {
  schema() {
    table("lk_t").column("e").add({ type: t.text()${unique ? ".unique()" : ""} });
  },
};
`,
  );
}

function runCli(
  work: string,
  argv: string[],
  options: { schema: string; databaseUrl?: string },
): CliResult {
  const connection =
    options.databaseUrl === undefined ? [] : ["--database-url", options.databaseUrl];
  const result = spawnSync(
    process.execPath,
    [
      "--import",
      "tsx",
      CLI_BIN,
      ...argv,
      "--dir",
      join(work, "migrations"),
      ...connection,
      "--policy",
      join(work, "policy.toml"),
      "--registry",
      join(work, "registry.json"),
      "--schema",
      options.schema,
      "--owner-app",
      OWNER_APP,
    ],
    {
      cwd: work,
      encoding: "utf8",
      env: { ...process.env, ZERO_MIGRATE_ADDON_PATH: ADDON_PATH, DATABASE_URL: "" },
    },
  );
  return {
    code: result.status,
    text: `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim(),
  };
}

function assertLockAdvisory(result: CliResult, verb: string): void {
  assert.equal(result.code, 0, `${verb}: an advisory must not gate the command; ${result.text}`);
  assert.match(result.text, /advisory/i, `${verb}: the hazard must be identified as advisory`);

  const lockLines = result.text.split(/\r?\n/).filter((line) => /ACCESS\s+EXCLUSIVE/i.test(line));
  assert.ok(
    lockLines.length > 0,
    `${verb}: the advisory must name the ACCESS EXCLUSIVE lock; ${result.text}`,
  );
  assert.ok(
    lockLines.some((line) => /\blk_t\b/i.test(line) && /ADD\s+CONSTRAINT/i.test(line)),
    `${verb}: one advisory line must name table lk_t, statement kind ADD CONSTRAINT, ` +
      `and ACCESS EXCLUSIVE; got: ${lockLines.join(" | ")}`,
  );
}

function assertNoLockAdvisory(result: CliResult, verb: string): void {
  assert.equal(result.code, 0, `${verb}: a plain addColumn must still succeed; ${result.text}`);
  assert.doesNotMatch(
    result.text,
    /ACCESS\s+EXCLUSIVE|validating index/i,
    `${verb}: a plain addColumn must surface no locking advisory; ${result.text}`,
  );
}

function lint(unique: boolean): CliResult {
  return lintDialect(unique, "postgres");
}

function lintDialect(unique: boolean, dialect: "postgres" | "mysql" | "sqlite"): CliResult {
  const work = baseProject();
  try {
    addColumnMigration(work, unique);
    return runCli(work, ["lint", "--explain", "--dialect", dialect], {
      schema: "lint_locksurface",
    });
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

async function plan(t: TestContext, unique: boolean): Promise<CliResult | null> {
  const client = await connectLivePg(t);
  if (!client) return null;

  const schema = uniqueNamespace(unique ? "lock_adv_unique" : "lock_adv_plain");
  const work = baseProject();
  try {
    await client.query(`CREATE SCHEMA "${schema}"`);
    const created = runCli(work, ["apply"], { schema, databaseUrl: pgUrl() });
    assert.equal(
      created.code,
      0,
      `the earlier table-creation migration must apply before plan; ${created.text}`,
    );

    addColumnMigration(work, unique);
    return runCli(work, ["plan"], { schema, databaseUrl: pgUrl() });
  } finally {
    await client
      .query(
        `DROP SCHEMA IF EXISTS "${schema}" CASCADE;
         DROP SCHEMA IF EXISTS "${schema}_migrations" CASCADE`,
      )
      .catch(() => {});
    await client.end().catch(() => {});
    rmSync(work, { recursive: true, force: true });
  }
}

test("lint --explain surfaces the unique addColumn ACCESS EXCLUSIVE advisory without gating", () => {
  assertLockAdvisory(lint(true), "lint --explain");
});

test("lint --explain control: a plain addColumn surfaces no locking advisory", () => {
  assertNoLockAdvisory(lint(false), "lint --explain");
});

test("plan surfaces the unique addColumn ACCESS EXCLUSIVE advisory without gating", async (t) => {
  const result = await plan(t, true);
  if (result) assertLockAdvisory(result, "plan");
});

test("plan control: a plain addColumn surfaces no locking advisory", async (t) => {
  const result = await plan(t, false);
  if (result) assertNoLockAdvisory(result, "plan");
});

test("F657: a dialect the analyzer cannot read says so instead of reporting clean", () => {
  // The analyzer parses PostgreSQL. MySQL renders identifiers with backticks, so
  // every statement fails to parse and the rule set returns nothing -- for SQL
  // this engine emits and is about to run. The report then read exactly like a
  // clean one, and an operator had no way to tell "looked, found nothing" from
  // "could not read any of this".
  //
  // The fix is not to invent MySQL coverage: MySQL 8 can often add a unique index
  // INPLACE where PostgreSQL takes ACCESS EXCLUSIVE, so emitting the PostgreSQL
  // advisory there would be wrong. The fix is that silence must be attributable.
  for (const dialect of ["mysql", "sqlite"] as const) {
    // A PLAIN addColumn, which MySQL accepts. The notice is about analyzer
    // COVERAGE, not about any one statement, so it must appear whether or not a
    // rule would have fired -- and this fixture keeps the arm from depending on
    // MySQL accepting a unique TEXT key, which it refuses for its own reasons.
    const result = lintDialect(false, dialect);
    assert.equal(result.code, 0, `${dialect}: a notice must not gate; ${result.text}`);
    assert.match(
      result.text,
      /analyzer_dialect_unsupported/,
      `${dialect}: an empty advisory list here means UNCHECKED, and the report ` +
        `must say which one it is; ${result.text}`,
    );
  }
});

test("CONTROL: PostgreSQL still evaluates rules rather than announcing it cannot", () => {
  // Without this, a fix that declared every dialect unanalyzable would satisfy
  // the arm above while destroying the advisory surface F650 added.
  const result = lintDialect(true, "postgres");
  assert.doesNotMatch(result.text, /analyzer_dialect_unsupported/);
  assert.match(result.text, /ACCESS\s+EXCLUSIVE/i);
});

test("F659: --json carries the advisories too, not only the human rendering", () => {
  // The advisory surface was added to the human output of `lint --explain` and
  // `plan`. Both verbs also have a `--json` shape, and that is what a CI gate
  // reads - which is precisely where a warning about a table-wide lock has to
  // land. A payload that omits them looks clean to the consumer least able to
  // notice it is missing.
  const work = baseProject();
  try {
    addColumnMigration(work, true);
    const result = runCli(work, ["lint", "--explain", "--json", "--dialect", "postgres"], {
      schema: "lint_locksurface_json",
    });
    assert.equal(result.code, 0, `an advisory must not gate; ${result.text}`);
    // CliResult folds stdout and stderr into one string, so slice out the JSON
    // document rather than parsing whatever the devShell banner printed first.
    const body = result.text.slice(result.text.indexOf("{"), result.text.lastIndexOf("}") + 1);
    const payload: unknown = JSON.parse(body);
    assert.ok(
      typeof payload === "object" && payload !== null && "advisories" in payload,
      `the JSON shape must carry advisories; got ${result.text}`,
    );
    const { advisories } = payload as { advisories: Array<{ message: string }> };
    assert.ok(
      advisories.some((advisory) => /ACCESS\s+EXCLUSIVE/i.test(advisory.message)),
      `and the lock must be among them; got ${JSON.stringify(advisories)}`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
